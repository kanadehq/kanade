//! Inventory facts read API. Three shapes:
//!
//!   * `GET /api/inventory/<pc_id>` — every probe's facts for one
//!     PC. Drives the SPA's detail view (vertical field/value).
//!   * `GET /api/inventory/by-job/<manifest_id>` — one probe's facts
//!     across every PC that's reported it. Drives the SPA's fleet
//!     list (row per PC, columns = summary fields).
//!   * `GET /api/inventory/jobs` — fleet-wide listing of inventory-
//!     tagged manifests, with both display + summary configs inline.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use kanade_shared::kv::BUCKET_JOBS;
use kanade_shared::manifest::{DisplayField, ExplodeSpec, InventoryHint, Manifest};
use serde::Serialize;
use sqlx::{AssertSqlSafe, Row};
use tracing::warn;

use crate::projector::explode::validate_ident;

use super::AppState;
use super::time_bounds::bounds_in_range;

#[derive(Serialize)]
pub struct InventoryFact {
    pub job_id: String,
    pub facts: serde_json::Value,
    pub display: Vec<DisplayField>,
    /// Optional fleet-list columns. Falls back to `display` in the
    /// SPA when omitted by the manifest.
    pub summary: Option<Vec<DisplayField>>,
    pub collected_at: Option<DateTime<Utc>>,
    pub recorded_at: Option<DateTime<Utc>>,
}

pub async fn list_for_pc(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<Json<Vec<InventoryFact>>, (StatusCode, String)> {
    let rows = sqlx::query(
        "SELECT job_id, facts_json, display_json, summary_json,
                collected_at, recorded_at
         FROM inventory_facts
         WHERE pc_id = ?
         ORDER BY job_id",
    )
    .bind(&pc_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, %pc_id, "inventory_facts query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let facts: Vec<InventoryFact> = rows.into_iter().map(row_to_fact).collect();
    Ok(Json(facts))
}

#[derive(Serialize)]
pub struct InventoryRow {
    pub pc_id: String,
    pub facts: serde_json::Value,
    pub collected_at: Option<DateTime<Utc>>,
    /// Account last seen on this PC, LEFT JOINed from the `agents`
    /// baseline row (maintained by the heartbeat projector, ~30 s
    /// cadence). Shown next to each PC's inventory facts so an operator
    /// can tell who uses a machine without cross-referencing the
    /// Agents page. `None` when the PC has no agent row yet or no
    /// sign-in has been recorded (non-Windows / pre-#655 agents).
    pub last_logon_user: Option<String>,
    pub last_logon_display_name: Option<String>,
}

#[derive(Serialize)]
pub struct InventoryByJob {
    pub manifest_id: String,
    pub display: Vec<DisplayField>,
    pub summary: Option<Vec<DisplayField>>,
    pub rows: Vec<InventoryRow>,
    /// #494: total matching PCs (pre-LIMIT) so the SPA can paginate.
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// #494: paging knobs for the by-job fleet list. Pre-fix the handler
/// returned the complete `facts_json` for every PC that ever
/// reported the probe — at fleet scale that materialises the whole
/// fleet's facts in memory and ships a response that can run to
/// hundreds of MB, per SPA poll, per probe.
#[derive(serde::Deserialize)]
pub struct ByJobParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Optional pc_id substring filter (case-insensitive LIKE).
    pub q: Option<String>,
}

const BY_JOB_DEFAULT_LIMIT: i64 = 100;
const BY_JOB_MAX_LIMIT: i64 = 1000;

pub async fn list_for_job(
    State(state): State<AppState>,
    Path(manifest_id): Path<String>,
    Query(params): Query<ByJobParams>,
) -> Result<Json<InventoryByJob>, (StatusCode, String)> {
    let limit = params
        .limit
        .unwrap_or(BY_JOB_DEFAULT_LIMIT)
        .clamp(1, BY_JOB_MAX_LIMIT);
    let offset = params.offset.unwrap_or(0).max(0);
    // Escape LIKE wildcards so a literal `%`/`_` in the filter
    // matches itself (same escaping discipline as api/results.rs).
    let like = params.q.as_deref().filter(|q| !q.is_empty()).map(|q| {
        format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inventory_facts
          WHERE job_id = ?
            AND (?2 IS NULL OR pc_id LIKE ?2 ESCAPE '\\')",
    )
    .bind(&manifest_id)
    .bind(&like)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, %manifest_id, "inventory_facts by-job count");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // LEFT JOIN the agents baseline row so each PC's last-seen account
    // rides along with its facts. The account lives on `agents`
    // (heartbeat-maintained, single source of truth) — not in
    // inventory facts — so we join at read time keyed by pc_id rather
    // than re-collecting it per inventory job. The join is 1:0/1:1
    // (agents.pc_id is unique), so it doesn't change the row count the
    // `total` COUNT above reports.
    let rows = sqlx::query(
        "SELECT f.pc_id, f.facts_json, f.display_json, f.summary_json, f.collected_at,
                a.last_logon_user, a.last_logon_display_name
         FROM inventory_facts f
         LEFT JOIN agents a ON a.pc_id = f.pc_id
         WHERE f.job_id = ?
           AND (?2 IS NULL OR f.pc_id LIKE ?2 ESCAPE '\\')
         ORDER BY f.pc_id
         LIMIT ?3 OFFSET ?4",
    )
    .bind(&manifest_id)
    .bind(&like)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, %manifest_id, "inventory_facts by-job query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Pull display + summary from the first page row that has them;
    // every row for one manifest_id has the same snapshot, since
    // the projector writes them together at upsert. #494: with
    // paging/filtering the page can be empty while rows exist — fall
    // back to a one-row unfiltered probe so the column config
    // survives an empty page.
    let mut display = rows
        .iter()
        .find_map(|r| {
            r.try_get::<Option<String>, _>("display_json")
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<Vec<DisplayField>>(&s).ok())
        })
        .unwrap_or_default();
    let mut summary = rows.iter().find_map(|r| {
        r.try_get::<Option<String>, _>("summary_json")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Vec<DisplayField>>(&s).ok())
    });
    // `display.is_empty()` (not `rows.is_empty() && total > 0`):
    // a filtered-empty page has total == 0 yet still needs the
    // headers, and probing once for any existing row covers every
    // empty-page shape (review PR #552, gemini).
    if display.is_empty() {
        match sqlx::query(
            "SELECT display_json, summary_json FROM inventory_facts
              WHERE job_id = ? LIMIT 1",
        )
        .bind(&manifest_id)
        .fetch_optional(&state.pool)
        .await
        {
            Ok(Some(r)) => {
                display = r
                    .try_get::<Option<String>, _>("display_json")
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str::<Vec<DisplayField>>(&s).ok())
                    .unwrap_or_default();
                summary = r
                    .try_get::<Option<String>, _>("summary_json")
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str::<Vec<DisplayField>>(&s).ok());
            }
            Ok(None) => {} // job never reported — genuinely no config
            Err(e) => {
                warn!(error = %e, %manifest_id, "inventory_facts fallback config probe failed");
            }
        }
    }

    let inv_rows: Vec<InventoryRow> = rows
        .into_iter()
        .map(|r| InventoryRow {
            pc_id: r.try_get("pc_id").unwrap_or_default(),
            facts: r
                .try_get::<String, _>("facts_json")
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null),
            collected_at: r.try_get("collected_at").ok(),
            last_logon_user: r
                .try_get::<Option<String>, _>("last_logon_user")
                .ok()
                .flatten(),
            last_logon_display_name: r
                .try_get::<Option<String>, _>("last_logon_display_name")
                .ok()
                .flatten(),
        })
        .collect();

    Ok(Json(InventoryByJob {
        manifest_id,
        display,
        summary,
        rows: inv_rows,
        total,
        limit,
        offset,
    }))
}

/// `GET /api/inventory/jobs` — list every inventory-tagged schedule
/// in the fleet (one row per manifest.id that has an `inventory:`
/// hint). The SPA Inventory page uses this to render a list of
/// probes even before any PC has reported facts.
#[derive(Serialize)]
pub struct InventoryJob {
    pub manifest_id: String,
    pub description: Option<String>,
    pub display: Vec<DisplayField>,
    pub summary: Option<Vec<DisplayField>>,
    /// v0.35 / #87: included so the SPA Software page knows which
    /// fields are searchable (one tab per element) and what
    /// columns / kinds each spec has (drives the filter chip row),
    /// without a separate per-manifest endpoint.
    pub explode: Option<Vec<ExplodeSpec>>,
    /// Reported-PC count for this manifest, joined from
    /// `inventory_facts` in one grouped query (see `list_jobs`). The
    /// SPA fleet sidebar shows this as a per-type device tally so the
    /// operator can pick a probe without scrolling every table.
    pub pc_count: i64,
}

pub async fn list_jobs(
    State(state): State<AppState>,
) -> Result<Json<Vec<InventoryJob>>, (StatusCode, String)> {
    let kv = state
        .jetstream
        .get_key_value(BUCKET_JOBS)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("get KV {BUCKET_JOBS}: {e}"),
            )
        })?;
    let mut out = Vec::new();
    let mut keys = match kv.keys().await {
        Ok(k) => k,
        Err(_) => return Ok(Json(out)),
    };
    while let Some(key) = keys.next().await {
        let key = match key {
            Ok(k) => k,
            Err(_) => continue,
        };
        let entry = match kv.get(&key).await.unwrap_or(None) {
            Some(b) => b,
            None => continue,
        };
        let job: Manifest = match serde_json::from_slice(&entry) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if let Some(hint) = job.inventory {
            out.push(InventoryJob {
                manifest_id: job.id,
                description: job.description,
                display: hint.display,
                summary: hint.summary,
                explode: hint.explode,
                pc_count: 0,
            });
        }
    }

    // Attach the reported-PC tally per manifest with a single grouped
    // query rather than an N+1 of `by-job?limit=1` calls from the SPA.
    // A failed/missing count is non-fatal: the sidebar still lists the
    // probe, just with a 0 tally, so a transient DB hiccup never blanks
    // the whole inventory nav. COUNT(DISTINCT pc_id) (not COUNT(*))
    // makes the *device* intent explicit so the badge can't silently
    // inflate if a retry / dual-write ever lands two rows for the same
    // (job_id, pc_id) pair.
    let counts: std::collections::HashMap<String, i64> = sqlx::query(
        "SELECT job_id, COUNT(DISTINCT pc_id) AS n FROM inventory_facts GROUP BY job_id",
    )
    .fetch_all(&state.pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("job_id").ok()?;
                let n: i64 = r.try_get("n").ok()?;
                Some((id, n))
            })
            .collect()
    })
    .unwrap_or_else(|e| {
        warn!(error = %e, "inventory jobs pc_count query");
        std::collections::HashMap::new()
    });
    for job in &mut out {
        job.pc_count = counts.get(&job.manifest_id).copied().unwrap_or(0);
    }

    out.sort_by(|a, b| a.manifest_id.cmp(&b.manifest_id));
    Ok(Json(out))
}

/// `GET /api/inventory/{manifest_id}/search/{field}` — cross-PC query
/// over the derived table for `field` (an `explode` spec on the
/// manifest). Filters come as query params; column names are
/// validated against the spec so operator typos / injection
/// attempts produce a clean 400 instead of an opaque SQL error.
///
/// Filter syntax (Django-ish):
///   * `<col>=<value>`        — exact match (eq)
///   * `<col>__contains=<v>`  — LIKE '%v%'
///   * `<col>__prefix=<v>`    — LIKE 'v%'
///   * `<col>__lt=<v>`        — strictly less than (lexical for TEXT, numeric for INTEGER/REAL)
///   * `<col>__le=<v>`, `__gt`, `__ge`, `__ne` — analogous
///
/// Response: per-row `{ pc_id, collected_at, <columns from spec> }`.
/// Up to 1000 rows; operator-side filters should narrow further.
pub async fn search(
    State(state): State<AppState>,
    Path((manifest_id, field)): Path<(String, String)>,
    Query(filters): Query<HashMap<String, String>>,
) -> Result<Json<Vec<serde_json::Map<String, serde_json::Value>>>, (StatusCode, String)> {
    let spec = load_explode_spec(&state, &manifest_id, &field).await?;
    // CodeRabbit #85 fix: a job registered AFTER backend startup has
    // a manifest in BUCKET_JOBS but no derived table yet (the
    // startup prewarm only covers manifests present at boot). The
    // first search query for such a job would fall through to "no
    // such table" SQL error → 500. Idempotent ensure_table_cached
    // here turns that case into a clean empty result instead. After
    // the first result delivery the projector will populate the
    // table for real.
    crate::projector::explode::ensure_table_cached(&state.pool, &spec)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("ensure derived table: {e}"),
            )
        })?;

    // Build the SELECT column list from the spec — never splice
    // operator-supplied identifiers into SQL. validate_ident
    // already ran at table-creation time but recheck defensively.
    validate_ident(&spec.table)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("table name: {e}")))?;
    for col in &spec.columns {
        validate_ident(&col.field)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("column name: {e}")))?;
    }

    // Gemini #85 fix: quote every operator-supplied identifier
    // with double quotes so SQL reserved words (`order`, `group`,
    // ...) work as column / table names. validate_ident already
    // rejected non-alphanumeric chars, so the quoted form is just
    // be syntactically safe against the reserved-word list.
    let column_csv = spec
        .columns
        .iter()
        .map(|c| format!("\"{}\"", c.field))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "SELECT pc_id, collected_at, {column_csv} FROM \"{}\"",
        spec.table
    );
    let mut binds: Vec<String> = Vec::new();
    let mut sep = " WHERE ";

    // Parse filters and build WHERE clauses.
    for (raw_key, value) in &filters {
        // Skip the pagination meta-params (handled below).
        if raw_key == "limit" || raw_key == "offset" {
            continue;
        }
        let (col, op) = match raw_key.split_once("__") {
            Some((c, o)) => (c.to_string(), o),
            None => (raw_key.clone(), "eq"),
        };
        // Reject filters on columns that don't exist on this spec.
        if !spec.columns.iter().any(|c| c.field == col) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown column for filter: {col:?}"),
            ));
        }
        validate_ident(&col).map_err(|e| (StatusCode::BAD_REQUEST, format!("column: {e}")))?;
        // CodeRabbit #85 fix: `%` and `_` inside operator-supplied
        // filter values must NOT be treated as SQL LIKE wildcards.
        // `model__contains=100%` previously matched "100" + anything,
        // not literally "100%". Escape backslash / `%` / `_` and add
        // an `ESCAPE '\'` clause to the LIKE variants. eq / lt / gt
        // etc. don't use LIKE semantics so they bind raw.
        let escape_like = |s: &str| -> String {
            s.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        };
        let (comparator, bound_value) = match op {
            "eq" => ("=", value.clone()),
            "ne" => ("<>", value.clone()),
            "lt" => ("<", value.clone()),
            "le" => ("<=", value.clone()),
            "gt" => (">", value.clone()),
            "ge" => (">=", value.clone()),
            "contains" => ("LIKE_ESC", format!("%{}%", escape_like(value))),
            "prefix" => ("LIKE_ESC", format!("{}%", escape_like(value))),
            "suffix" => ("LIKE_ESC", format!("%{}", escape_like(value))),
            other => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("unknown filter operator {other:?}"),
                ));
            }
        };
        sql.push_str(sep);
        if comparator == "LIKE_ESC" {
            // SQLite needs the ESCAPE clause set explicitly when
            // backslash is the escape char — there's no default.
            sql.push_str(&format!("\"{col}\" LIKE ? ESCAPE '\\'"));
        } else {
            sql.push_str(&format!("\"{col}\" {comparator} ?"));
        }
        binds.push(bound_value);
        sep = " AND ";
    }
    // Gemini #85 fix: take `limit` + `offset` from query params for
    // basic pagination. Defaults preserve the original 1000-row cap.
    // Hard ceiling at 5000 — operators should narrow filters
    // instead of paginating through huge result sets.
    let limit: u32 = filters
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
        .min(5000);
    let offset: u32 = filters
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    sql.push_str(&format!(
        " ORDER BY pc_id, collected_at DESC LIMIT {limit} OFFSET {offset}"
    ));

    let mut q = sqlx::query(AssertSqlSafe(sql));
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(&state.pool).await.map_err(|e| {
        warn!(error = %e, manifest_id, field, "explode search query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let mut out: Vec<serde_json::Map<String, serde_json::Value>> = Vec::with_capacity(rows.len());
    for r in rows {
        let mut map = serde_json::Map::new();
        if let Ok(pc_id) = r.try_get::<String, _>("pc_id") {
            map.insert("pc_id".into(), serde_json::Value::String(pc_id));
        }
        if let Ok(Some(t)) = r.try_get::<Option<DateTime<Utc>>, _>("collected_at") {
            map.insert(
                "collected_at".into(),
                serde_json::Value::String(t.to_rfc3339()),
            );
        }
        for col in &spec.columns {
            // Gemini #85 fix: decode by declared type instead of
            // try-string-first fallback. Pre-fix the path was
            // 3 attempted decodes (String → i64 → f64) with sqlx
            // errors as flow control — wasteful when col.kind tells
            // us the column type up-front.
            let v: serde_json::Value = match col.kind.as_deref() {
                Some("integer") => r
                    .try_get::<Option<i64>, _>(col.field.as_str())
                    .ok()
                    .flatten()
                    .map(|i| serde_json::Value::Number(i.into()))
                    .unwrap_or(serde_json::Value::Null),
                Some("real") => r
                    .try_get::<Option<f64>, _>(col.field.as_str())
                    .ok()
                    .flatten()
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                _ => r
                    .try_get::<Option<String>, _>(col.field.as_str())
                    .ok()
                    .flatten()
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            };
            map.insert(col.field.clone(), v);
        }
        out.push(map);
    }
    enrich_with_account(&state.pool, &mut out).await;
    Ok(Json(out))
}

/// Keys under which [`enrich_with_account`] injects the per-PC account
/// into a cross-PC search result row. The leading `@` is deliberate:
/// every explode / scalar column name passes `validate_ident` (which
/// permits only `[A-Za-z0-9_]`), so a `@`-prefixed key can never
/// collide with an operator-defined column — the enrichment is always
/// purely additive, never clobbering a real fact.
const ACCOUNT_USER_KEY: &str = "@account_user";
const ACCOUNT_DISPLAY_NAME_KEY: &str = "@account_display_name";

/// Enrich cross-PC inventory search rows with the account last seen on
/// each PC. The account (`last_logon_user` / `last_logon_display_name`)
/// is a per-PC baseline fact kept on the `agents` row by the heartbeat
/// projector (~30 s cadence) — fresher than any inventory schedule and
/// a single source of truth — so we join it in at read time keyed by
/// `pc_id` rather than re-collecting it per inventory job. The lookup
/// is chunked so it stays under SQLite's bind-variable ceiling even at
/// the 5000-row search cap (see below).
///
/// Best-effort: a query failure logs and leaves the rows unenriched
/// rather than failing the search the operator actually asked for.
/// Rows whose PC has no agent row (or a NULL account) still get the
/// keys set to JSON null, so the SPA renders a stable column.
async fn enrich_with_account(
    pool: &sqlx::SqlitePool,
    rows: &mut [serde_json::Map<String, serde_json::Value>],
) {
    use std::collections::{BTreeSet, HashMap};

    // Distinct pc_ids present in this page (BTreeSet dedupes + gives a
    // stable bind order).
    let pc_ids: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.get("pc_id") {
            Some(serde_json::Value::String(p)) => Some(p.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut by_pc: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    if pc_ids.is_empty() {
        // No string pc_id on any row (empty page, or a decode miss):
        // still run apply_account with an empty map so every row gets
        // the `@account_*` keys set to null. This keeps the documented
        // "keys are always present" invariant true unconditionally,
        // rather than only when the agents lookup actually ran (Claude
        // review #729).
        apply_account(rows, &by_pc);
        return;
    }
    // Gemini #729: the scalar search is one row per PC and caps at 5000
    // rows, so `pc_ids` can exceed SQLite's default host-parameter limit
    // (`SQLITE_MAX_VARIABLE_NUMBER` — 999 on older builds; bundled
    // builds raise it to 32766, but we don't control the runtime's
    // SQLite). Chunk the `IN (...)` lookup at 999 so we never trip
    // `too many SQL variables` regardless of the build.
    for chunk in pc_ids.chunks(999) {
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT pc_id, last_logon_user, last_logon_display_name \
               FROM agents WHERE pc_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(id);
        }
        qb.push(")");

        let account_rows = match qb.build().fetch_all(pool).await {
            Ok(rs) => rs,
            Err(e) => {
                warn!(error = %e, "inventory account enrichment query");
                return;
            }
        };
        for r in account_rows {
            let Ok(pc) = r.try_get::<String, _>("pc_id") else {
                continue;
            };
            let user = r
                .try_get::<Option<String>, _>("last_logon_user")
                .ok()
                .flatten();
            let name = r
                .try_get::<Option<String>, _>("last_logon_display_name")
                .ok()
                .flatten();
            by_pc.insert(pc, (user, name));
        }
    }

    apply_account(rows, &by_pc);
}

/// Inject the per-PC account into each row under the `@account_*` keys
/// (pure; the DB lookup lives in [`enrich_with_account`]). Split out so
/// the namespacing / null-fallback behaviour is unit-testable without a
/// pool. A row whose pc_id isn't in `by_pc` (or maps to a NULL account)
/// gets both keys set to JSON null, so the column is always present.
fn apply_account(
    rows: &mut [serde_json::Map<String, serde_json::Value>],
    by_pc: &std::collections::HashMap<String, (Option<String>, Option<String>)>,
) {
    let to_json = |s: Option<String>| {
        s.map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null)
    };
    for r in rows.iter_mut() {
        let (user, name) = match r.get("pc_id") {
            Some(serde_json::Value::String(p)) => by_pc.get(p).cloned().unwrap_or((None, None)),
            _ => (None, None),
        };
        r.insert(ACCOUNT_USER_KEY.into(), to_json(user));
        r.insert(ACCOUNT_DISPLAY_NAME_KEY.into(), to_json(name));
    }
}

/// One searchable top-level scalar fact, derived from an
/// [`InventoryHint`]'s `display` list. `numeric` decides whether
/// comparison filters CAST the JSON value to REAL (so `ram_bytes <
/// 8e9` compares as a number) or compare it as text.
struct ScalarColumn {
    field: String,
    numeric: bool,
}

/// #574: the set of top-level scalar facts an operator can filter on
/// for `manifest_id`. Pulled from the manifest's `inventory.display`
/// list — every display field EXCEPT the `kind: "table"` ones, which
/// are arrays handled by `explode` sub-tables, not scalars. A
/// `number` / `bytes` render hint marks the column numeric so the
/// search builds a numeric comparison.
fn scalar_columns(hint: &InventoryHint) -> Vec<ScalarColumn> {
    hint.display
        .iter()
        .filter(|d| d.kind.as_deref() != Some("table"))
        .map(|d| ScalarColumn {
            field: d.field.clone(),
            numeric: matches!(d.kind.as_deref(), Some("number") | Some("bytes")),
        })
        .collect()
}

/// `GET /api/inventory/{manifest_id}/search-scalars` — cross-PC query
/// over the **top-level scalar facts** stored in
/// `inventory_facts.facts_json`, with NO `explode` sub-table required
/// (#574). Mirrors [`search`]'s Django-ish filter syntax, but each
/// `<col>` must be a non-`table` `display` field on the manifest and
/// the WHERE clause runs against `json_extract(facts_json, '$.<col>')`
/// instead of a derived table column.
///
/// Filter syntax (same as [`search`]):
///   * `<col>=<value>`        — exact match (eq)
///   * `<col>__contains=<v>`  — LIKE '%v%'
///   * `<col>__prefix=<v>`    — LIKE 'v%'
///   * `<col>__suffix=<v>`    — LIKE '%v'
///   * `<col>__lt|le|gt|ge|ne=<v>` — comparators (numeric for
///     `number`/`bytes` columns, lexical otherwise)
///
/// Response: per-row `{ pc_id, collected_at, <each scalar field> }`.
pub async fn search_scalars(
    State(state): State<AppState>,
    Path(manifest_id): Path<String>,
    Query(filters): Query<HashMap<String, String>>,
) -> Result<Json<Vec<serde_json::Map<String, serde_json::Value>>>, (StatusCode, String)> {
    let hint = load_inventory_hint(&state, &manifest_id).await?;
    let scalars = scalar_columns(&hint);
    if scalars.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("manifest {manifest_id:?} has no scalar display fields to search"),
        ));
    }
    // validate_ident every field name before it's spliced into a
    // json_extract path — the path can't be bound, only the value.
    for s in &scalars {
        validate_ident(&s.field)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("display field name: {e}")))?;
    }

    // Gemini #669 fix: let SQLite project just the requested scalar
    // fields into a tiny JSON object instead of shipping the entire
    // `facts_json` (which also carries every non-exploded array) over
    // the connection and re-parsing it per row. At fleet scale (up to
    // 5000 rows) that's a large CPU/memory saving. Field names are
    // validate_ident'd above, so splicing them into the json_object
    // keys + json_extract paths is safe.
    let projection = scalars
        .iter()
        .map(|s| format!("'{0}', json_extract(facts_json, '$.{0}')", s.field))
        .collect::<Vec<_>>()
        .join(", ");
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT pc_id, collected_at, json_object({projection}) AS projected_json \
         FROM inventory_facts WHERE job_id = "
    ));
    qb.push_bind(&manifest_id);

    let escape_like = |s: &str| -> String {
        s.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    };
    for (raw_key, value) in &filters {
        if raw_key == "limit" || raw_key == "offset" {
            continue;
        }
        let (col, op) = match raw_key.split_once("__") {
            Some((c, o)) => (c.to_string(), o),
            None => (raw_key.clone(), "eq"),
        };
        let scalar = scalars.iter().find(|s| s.field == col).ok_or((
            StatusCode::BAD_REQUEST,
            format!("unknown column for filter: {col:?}"),
        ))?;
        // col == scalar.field, already validate_ident'd above — safe
        // to splice into the JSON path. Values always go through bind.
        match op {
            "eq" | "ne" | "lt" | "le" | "gt" | "ge" => {
                let comparator = match op {
                    "eq" => "=",
                    "ne" => "<>",
                    "lt" => "<",
                    "le" => "<=",
                    "gt" => ">",
                    "ge" => ">=",
                    _ => unreachable!(),
                };
                if scalar.numeric {
                    // CAST both sides to REAL: json_extract yields a
                    // typed value (INTEGER/REAL/TEXT) and SQLite's
                    // storage-class ordering would otherwise sort any
                    // number before any text, breaking `< '120'`.
                    let num: f64 = value.parse().map_err(|_| {
                        (
                            StatusCode::BAD_REQUEST,
                            format!(
                                "filter on numeric column {col:?} needs a number, got {value:?}"
                            ),
                        )
                    })?;
                    qb.push(format!(
                        " AND CAST(json_extract(facts_json, '$.{col}') AS REAL) {comparator} "
                    ));
                    qb.push_bind(num);
                } else {
                    qb.push(format!(
                        " AND json_extract(facts_json, '$.{col}') {comparator} "
                    ));
                    qb.push_bind(value.clone());
                }
            }
            "contains" | "prefix" | "suffix" => {
                let pattern = match op {
                    "contains" => format!("%{}%", escape_like(value)),
                    "prefix" => format!("{}%", escape_like(value)),
                    "suffix" => format!("%{}", escape_like(value)),
                    _ => unreachable!(),
                };
                qb.push(format!(" AND json_extract(facts_json, '$.{col}') LIKE "));
                qb.push_bind(pattern);
                qb.push(" ESCAPE '\\'");
            }
            other => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("unknown filter operator {other:?}"),
                ));
            }
        }
    }

    let limit: i64 = filters
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
        .clamp(1, 5000);
    let offset: i64 = filters
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .max(0);
    qb.push(" ORDER BY pc_id LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);

    let rows = qb.build().fetch_all(&state.pool).await.map_err(|e| {
        warn!(error = %e, manifest_id, "scalar inventory search query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let mut out: Vec<serde_json::Map<String, serde_json::Value>> = Vec::with_capacity(rows.len());
    for r in rows {
        let mut map = serde_json::Map::new();
        if let Ok(pc_id) = r.try_get::<String, _>("pc_id") {
            map.insert("pc_id".into(), serde_json::Value::String(pc_id));
        }
        if let Ok(Some(t)) = r.try_get::<Option<DateTime<Utc>>, _>("collected_at") {
            map.insert(
                "collected_at".into(),
                serde_json::Value::String(t.to_rfc3339()),
            );
        }
        // The SQL already projected just the scalar fields into a
        // small JSON object, so parse that directly — a missing key
        // (json_extract → NULL) becomes an explicit null cell.
        let projected: serde_json::Map<String, serde_json::Value> = r
            .try_get::<String, _>("projected_json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for s in &scalars {
            let v = projected
                .get(&s.field)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            map.insert(s.field.clone(), v);
        }
        out.push(map);
    }
    enrich_with_account(&state.pool, &mut out).await;
    Ok(Json(out))
}

/// #574: resolve a manifest's [`InventoryHint`] for the scalar search,
/// preferring the warm spec cache (shared with the explode path) and
/// falling back to the KV fetch on a miss. Returns 404 for an
/// unknown manifest / a manifest with no `inventory:` block.
async fn load_inventory_hint(
    state: &AppState,
    manifest_id: &str,
) -> Result<InventoryHint, (StatusCode, String)> {
    if let Some(m) = state.explode_spec_cache.manifest(manifest_id).await {
        return m.inventory.clone().ok_or((
            StatusCode::NOT_FOUND,
            format!("manifest {manifest_id:?} has no inventory hint"),
        ));
    }

    let kv = state
        .jetstream
        .get_key_value(BUCKET_JOBS)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("jobs KV: {e}")))?;
    let entry = kv
        .get(manifest_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("manifest {manifest_id:?} not registered"),
        ))?;
    let manifest: Manifest = serde_json::from_slice(&entry).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("parse manifest: {e}"),
        )
    })?;
    let hint = manifest.inventory.clone().ok_or((
        StatusCode::NOT_FOUND,
        format!("manifest {manifest_id:?} has no inventory hint"),
    ))?;
    state.explode_spec_cache.insert_manifest(manifest).await;
    Ok(hint)
}

/// Fetch one manifest's [`ExplodeSpec`] by field name. Returns
/// 404 for unknown manifest / unknown field so the caller doesn't
/// have to disambiguate.
///
/// v0.35 / #88: in-memory cache (kept fresh by a KV `watch_all()`
/// on `BUCKET_JOBS`) is consulted first. Cache hit avoids the
/// ~30 ms NATS KV round-trip per search request — load-bearing
/// for the SPA Software page (#87) where each filter-chip
/// keystroke fires a request. Cold-cache miss / startup race /
/// watcher fell behind all fall back to the KV path below and
/// repopulate the cache on success.
async fn load_explode_spec(
    state: &AppState,
    manifest_id: &str,
    field: &str,
) -> Result<ExplodeSpec, (StatusCode, String)> {
    if let Some(hit) = state.explode_spec_cache.get(manifest_id, field).await {
        return Ok(hit);
    }

    let kv = state
        .jetstream
        .get_key_value(BUCKET_JOBS)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("jobs KV: {e}")))?;
    let entry = kv
        .get(manifest_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("manifest {manifest_id:?} not registered"),
        ))?;
    let manifest: Manifest = serde_json::from_slice(&entry).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("parse manifest: {e}"),
        )
    })?;
    let specs = manifest
        .inventory
        .as_ref()
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("manifest {manifest_id:?} has no inventory hint"),
        ))?
        .explode
        .clone()
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("manifest {manifest_id:?} has no explode specs"),
        ))?;
    // Populate the cache before answering — subsequent requests
    // for any field on this manifest go straight through. #488:
    // store the whole manifest so the results projector's hint
    // lookups share the warm entry.
    state.explode_spec_cache.insert_manifest(manifest).await;
    specs.into_iter().find(|s| s.field == field).ok_or((
        StatusCode::NOT_FOUND,
        format!("manifest {manifest_id:?} has no explode field {field:?}"),
    ))
}

/// `GET /api/inventory/{manifest_id}/history/pc/{pc_id}` — per-PC
/// timeline from `inventory_history` (#41). Optional query params:
/// `field` (narrow to one explode field), `since` (ISO-8601 lower
/// bound on observed_at), `limit` (default 500, ceiling 5000).
#[derive(Serialize)]
pub struct HistoryEventRow {
    pub id: i64,
    pub pc_id: String,
    pub job_id: String,
    pub field_path: String,
    pub identity_json: Option<String>,
    pub change_kind: String,
    pub before_json: Option<String>,
    pub after_json: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
}

#[derive(serde::Deserialize)]
pub struct HistoryParams {
    pub field: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
}

pub async fn history_for_pc(
    State(state): State<AppState>,
    Path((manifest_id, pc_id)): Path<(String, String)>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Vec<HistoryEventRow>>, (StatusCode, String)> {
    // Issue #1126: `observed_at` is a string-stored timestamp compared
    // byte-wise, so an expanded-year `since` would sort below every row
    // and match the whole table instead of a window. Reject it.
    if !bounds_in_range([params.since]) {
        return Err((
            StatusCode::BAD_REQUEST,
            "since: year out of range (must be 0..=9999)".into(),
        ));
    }
    let limit = params.limit.unwrap_or(500).min(5000);
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, pc_id, job_id, field_path, identity_json, \
                change_kind, before_json, after_json, observed_at \
           FROM inventory_history \
          WHERE job_id = ",
    );
    qb.push_bind(manifest_id);
    qb.push(" AND pc_id = ");
    qb.push_bind(pc_id);
    if let Some(f) = params.field.filter(|s| !s.is_empty()) {
        qb.push(" AND field_path = ");
        qb.push_bind(f);
    }
    if let Some(t) = params.since {
        qb.push(" AND observed_at >= ");
        qb.push_bind(t);
    }
    qb.push(" ORDER BY observed_at DESC LIMIT ");
    qb.push_bind(limit as i64);

    let rows = qb.build().fetch_all(&state.pool).await.map_err(|e| {
        warn!(error = %e, "inventory_history per-pc query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Gemini #86 fix: non-nullable schema columns (id, pc_id,
    // job_id, field_path, change_kind) propagate decode errors
    // rather than silently `unwrap_or_default`-ing them. Schema
    // drift turns into a clean 500 with a diagnostic instead of
    // empty-string-laden rows hitting the SPA. Nullable columns
    // (identity_json / before / after / observed_at) keep
    // `.ok()` because NULL is a legitimate value.
    let out: Result<Vec<HistoryEventRow>, _> = rows
        .into_iter()
        .map(|r| {
            Ok::<_, sqlx::Error>(HistoryEventRow {
                id: r.try_get("id")?,
                pc_id: r.try_get("pc_id")?,
                job_id: r.try_get("job_id")?,
                field_path: r.try_get("field_path")?,
                identity_json: r.try_get("identity_json").ok(),
                change_kind: r.try_get("change_kind")?,
                before_json: r.try_get("before_json").ok(),
                after_json: r.try_get("after_json").ok(),
                observed_at: r.try_get("observed_at").ok(),
            })
        })
        .collect();
    let out = out.map_err(|e| {
        warn!(error = %e, "inventory_history row decode");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("decode history row: {e}"),
        )
    })?;
    Ok(Json(out))
}

/// v0.35 / #90: extract `identity.<key>=<value>` query params from a
/// flat HashMap and validate each `<key>` against the same
/// `[A-Za-z_][A-Za-z0-9_]{0,63}` shape we use for explode column
/// names. The validated key is then safe to splice into a
/// `json_extract(identity_json, '$.<key>')` SQL path — the path
/// can't be bound, only the value can.
fn parse_identity_filters(
    params: &HashMap<String, String>,
) -> Result<Vec<(String, String)>, (StatusCode, String)> {
    let mut out = Vec::new();
    for (k, v) in params {
        if let Some(field) = k.strip_prefix("identity.") {
            validate_ident(field)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("identity.{field}: {e}")))?;
            out.push((field.to_string(), v.clone()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// `GET /api/inventory/{manifest_id}/history/search` — fleet-wide
/// (cross-PC) timeline from `inventory_history` (#90). Same row
/// shape as `history_for_pc`, just unfiltered by pc_id so operators
/// can answer "which PCs had Chrome installed at any point in the
/// last 90 days?" / "did anyone roll Chrome back from 121 to 120?"
/// without iterating over PCs themselves.
///
/// Query params (all optional):
///   * `field=<spec.field>`       — narrow to one explode field
///   * `kind=added|removed|changed`
///   * `since=<ISO-8601>`         — observed_at >=
///   * `until=<ISO-8601>`         — observed_at <
///   * `identity.<key>=<value>`   — match against the JSON object
///     stored in `identity_json` (e.g. `identity.name=Chrome` for
///     `apps`-shape spec or `identity.device_id=C:` for `disks`).
///     Validated against the same identifier rules as explode
///     columns; splicing into the SQL `$.path` is safe.
///   * `limit` (default 500, ceiling 5000), `offset`
pub async fn fleet_history_search(
    State(state): State<AppState>,
    Path(manifest_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<HistoryEventRow>>, (StatusCode, String)> {
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(500)
        .min(5000);
    let offset = params
        .get("offset")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let kind = params.get("kind").filter(|s| !s.is_empty());
    if let Some(k) = kind
        && !matches!(k.as_str(), "added" | "removed" | "changed")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("kind must be one of added / removed / changed (got {k:?})"),
        ));
    }
    let since: Option<DateTime<Utc>> = params
        .get("since")
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<DateTime<Utc>>()
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("since: {e}")))
        })
        .transpose()?;
    let until: Option<DateTime<Utc>> = params
        .get("until")
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<DateTime<Utc>>()
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("until: {e}")))
        })
        .transpose()?;
    // Issue #1126: `observed_at` is a string-stored timestamp compared
    // byte-wise below, so an expanded-year bound would invert the window
    // instead of narrowing it. Reject before building the query.
    if !bounds_in_range([since]) {
        return Err((
            StatusCode::BAD_REQUEST,
            "since: year out of range (must be 0..=9999)".into(),
        ));
    }
    if !bounds_in_range([until]) {
        return Err((
            StatusCode::BAD_REQUEST,
            "until: year out of range (must be 0..=9999)".into(),
        ));
    }
    let field = params.get("field").filter(|s| !s.is_empty());
    let identity_filters = parse_identity_filters(&params)?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, pc_id, job_id, field_path, identity_json, \
                change_kind, before_json, after_json, observed_at \
           FROM inventory_history \
          WHERE job_id = ",
    );
    qb.push_bind(&manifest_id);
    if let Some(f) = field {
        qb.push(" AND field_path = ");
        qb.push_bind(f);
    }
    if let Some(k) = kind {
        qb.push(" AND change_kind = ");
        qb.push_bind(k);
    }
    if let Some(t) = since {
        qb.push(" AND observed_at >= ");
        qb.push_bind(t);
    }
    if let Some(t) = until {
        qb.push(" AND observed_at < ");
        qb.push_bind(t);
    }
    for (key, value) in &identity_filters {
        // key validated to [A-Za-z_][A-Za-z0-9_]{0,63} above, safe
        // to interpolate into the JSON path. Value goes through
        // bind so untrusted operator input never touches SQL text.
        qb.push(format!(" AND json_extract(identity_json, '$.{key}') = "));
        qb.push_bind(value);
    }
    qb.push(" ORDER BY observed_at DESC LIMIT ");
    qb.push_bind(limit as i64);
    qb.push(" OFFSET ");
    qb.push_bind(offset as i64);

    let rows = qb.build().fetch_all(&state.pool).await.map_err(|e| {
        warn!(error = %e, "inventory_history fleet query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let out: Result<Vec<HistoryEventRow>, _> = rows
        .into_iter()
        .map(|r| {
            Ok::<_, sqlx::Error>(HistoryEventRow {
                id: r.try_get("id")?,
                pc_id: r.try_get("pc_id")?,
                job_id: r.try_get("job_id")?,
                field_path: r.try_get("field_path")?,
                identity_json: r.try_get("identity_json").ok(),
                change_kind: r.try_get("change_kind")?,
                before_json: r.try_get("before_json").ok(),
                after_json: r.try_get("after_json").ok(),
                observed_at: r.try_get("observed_at").ok(),
            })
        })
        .collect();
    let out = out.map_err(|e| {
        warn!(error = %e, "fleet history row decode");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("decode history row: {e}"),
        )
    })?;
    Ok(Json(out))
}

/// `GET /api/inventory/{manifest_id}/history/first_seen` — for each
/// PC matching the identity filter (e.g. `identity.name=Chrome`),
/// return the earliest `observed_at` of any matching event. Drives
/// the rollout-curve chart's "% of fleet on X over time" view
/// without forcing the client to paginate /history/search and
/// dedupe by pc_id (which gets the ordering wrong across pages).
///
/// Query params:
///   * `field=<spec.field>`       — required-ish (the typical curve
///     is per-explode-field; the SQL still runs without it but the
///     results blend events across fields, which is rarely useful)
///   * `identity.<key>=<value>`   — at least one is typical
///     (otherwise every PC ever seen comes back); not enforced
///   * `since=<ISO-8601>`         — observed_at >=
///   * `limit` (default 5000, ceiling 5000), `offset` — pagination
///     for fleets exceeding 5000 PCs that match the identity filter
pub async fn first_seen(
    State(state): State<AppState>,
    Path(manifest_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<FirstSeenRow>>, (StatusCode, String)> {
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(5000)
        .min(5000);
    // Gemini #124 fix: pagination on first_seen too — fleets with
    // > 5000 PCs need offset to fetch the full curve. Mirrors the
    // fleet_history_search pagination shape so client logic is
    // identical across both endpoints.
    let offset = params
        .get("offset")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let field = params.get("field").filter(|s| !s.is_empty());
    let since: Option<DateTime<Utc>> = params
        .get("since")
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<DateTime<Utc>>()
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("since: {e}")))
        })
        .transpose()?;
    // Issue #1126: `observed_at` is a string-stored timestamp compared
    // byte-wise below, so an expanded-year `since` would sort below every
    // row and match the whole table instead of a window. Reject it.
    if !bounds_in_range([since]) {
        return Err((
            StatusCode::BAD_REQUEST,
            "since: year out of range (must be 0..=9999)".into(),
        ));
    }
    let identity_filters = parse_identity_filters(&params)?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT pc_id, MIN(observed_at) AS first_seen_at \
           FROM inventory_history \
          WHERE job_id = ",
    );
    qb.push_bind(&manifest_id);
    if let Some(f) = field {
        qb.push(" AND field_path = ");
        qb.push_bind(f);
    }
    if let Some(t) = since {
        qb.push(" AND observed_at >= ");
        qb.push_bind(t);
    }
    for (key, value) in &identity_filters {
        qb.push(format!(" AND json_extract(identity_json, '$.{key}') = "));
        qb.push_bind(value);
    }
    qb.push(" GROUP BY pc_id ORDER BY first_seen_at ASC LIMIT ");
    qb.push_bind(limit as i64);
    qb.push(" OFFSET ");
    qb.push_bind(offset as i64);

    let rows = qb.build().fetch_all(&state.pool).await.map_err(|e| {
        warn!(error = %e, "inventory_history first_seen query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let out: Result<Vec<FirstSeenRow>, _> = rows
        .into_iter()
        .map(|r| {
            Ok::<_, sqlx::Error>(FirstSeenRow {
                pc_id: r.try_get("pc_id")?,
                first_seen_at: r.try_get("first_seen_at").ok(),
            })
        })
        .collect();
    let out = out.map_err(|e| {
        warn!(error = %e, "first_seen row decode");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("decode first_seen row: {e}"),
        )
    })?;
    Ok(Json(out))
}

#[derive(Serialize)]
pub struct FirstSeenRow {
    pub pc_id: String,
    pub first_seen_at: Option<DateTime<Utc>>,
}

fn row_to_fact(r: sqlx::sqlite::SqliteRow) -> InventoryFact {
    let facts: serde_json::Value = r
        .try_get::<String, _>("facts_json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let display: Vec<DisplayField> = r
        .try_get::<Option<String>, _>("display_json")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let summary: Option<Vec<DisplayField>> = r
        .try_get::<Option<String>, _>("summary_json")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    InventoryFact {
        job_id: r.try_get("job_id").unwrap_or_default(),
        facts,
        display,
        summary,
        collected_at: r.try_get("collected_at").ok(),
        recorded_at: r.try_get("recorded_at").ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parse_identity_filters_empty_when_no_identity_prefix() {
        let got = parse_identity_filters(&params(&[
            ("field", "apps"),
            ("kind", "added"),
            ("since", "2026-04-01"),
        ]))
        .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn parse_identity_filters_extracts_pairs() {
        let got = parse_identity_filters(&params(&[
            ("identity.name", "Chrome"),
            ("identity.source", "appx"),
            ("field", "apps"),
        ]))
        .unwrap();
        // Sorted for stable assertion + stable SQL clause order.
        assert_eq!(
            got,
            vec![
                ("name".to_string(), "Chrome".to_string()),
                ("source".to_string(), "appx".to_string()),
            ]
        );
    }

    #[test]
    fn parse_identity_filters_rejects_injection_attempts() {
        // The key part splices into a json_extract path; validate_ident
        // is the choke point that keeps SQL injection unreachable.
        // A dotted / quoted key gets a clean 400 instead of a malformed
        // SQL surface.
        let err = parse_identity_filters(&params(&[("identity.name';--", "x")])).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_identity_filters_rejects_empty_field_name() {
        let err = parse_identity_filters(&params(&[("identity.", "x")])).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    fn display(field: &str, kind: Option<&str>) -> DisplayField {
        DisplayField {
            field: field.to_string(),
            label: field.to_string(),
            kind: kind.map(str::to_string),
            columns: None,
        }
    }

    #[test]
    fn scalar_columns_excludes_table_kind_and_marks_numeric() {
        // `apps` is a `kind: table` array exploded into a sub-table —
        // it must NOT appear as a searchable scalar. `ram_bytes` /
        // `cpu_count` carry numeric render hints, so they compare as
        // numbers; `os_build` / a hint-less field stay textual.
        let hint = InventoryHint {
            display: vec![
                display("pc_model", None),
                display("os_build", None),
                display("ram_bytes", Some("bytes")),
                display("cpu_count", Some("number")),
                display("installed_at", Some("timestamp")),
                display("apps", Some("table")),
            ],
            summary: None,
            explode: None,
            history_scalars: None,
        };
        let cols = scalar_columns(&hint);
        let names: Vec<&str> = cols.iter().map(|c| c.field.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "pc_model",
                "os_build",
                "ram_bytes",
                "cpu_count",
                "installed_at"
            ],
            "table-kind fields are dropped, order preserved"
        );
        let numeric: std::collections::HashMap<&str, bool> =
            cols.iter().map(|c| (c.field.as_str(), c.numeric)).collect();
        assert!(numeric["ram_bytes"]);
        assert!(numeric["cpu_count"]);
        assert!(!numeric["pc_model"]);
        assert!(!numeric["os_build"]);
        // `timestamp` is rendered specially but compares lexically
        // (ISO-8601 sorts correctly as text), so it's not numeric.
        assert!(!numeric["installed_at"]);
    }

    fn row(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn apply_account_injects_namespaced_keys() {
        use serde_json::Value;
        let mut rows = vec![row(&[
            ("pc_id", Value::String("PC-1".into())),
            // An operator-defined column literally named `last_logon_user`
            // must survive untouched — enrichment uses the `@`-prefixed
            // keys, which `validate_ident` can never produce.
            ("last_logon_user", Value::String("not-the-account".into())),
        ])];
        let mut by_pc = std::collections::HashMap::new();
        by_pc.insert(
            "PC-1".to_string(),
            (
                Some("CONTOSO\\jdoe".to_string()),
                Some("John Doe".to_string()),
            ),
        );

        apply_account(&mut rows, &by_pc);

        let r = &rows[0];
        assert_eq!(r[ACCOUNT_USER_KEY], Value::String("CONTOSO\\jdoe".into()));
        assert_eq!(
            r[ACCOUNT_DISPLAY_NAME_KEY],
            Value::String("John Doe".into())
        );
        // The collision-named real column is preserved.
        assert_eq!(
            r["last_logon_user"],
            Value::String("not-the-account".into())
        );
    }

    #[test]
    fn apply_account_nulls_when_pc_absent_or_account_empty() {
        use serde_json::Value;
        let mut rows = vec![
            row(&[("pc_id", Value::String("PC-unknown".into()))]),
            row(&[("pc_id", Value::String("PC-no-name".into()))]),
        ];
        let mut by_pc = std::collections::HashMap::new();
        // PC-no-name has a login but no display name; PC-unknown isn't
        // in the map at all.
        by_pc.insert(
            "PC-no-name".to_string(),
            (Some("WG\\kiosk".to_string()), None),
        );

        apply_account(&mut rows, &by_pc);

        // Unknown PC → both keys present, both null (stable SPA column).
        assert_eq!(rows[0][ACCOUNT_USER_KEY], Value::Null);
        assert_eq!(rows[0][ACCOUNT_DISPLAY_NAME_KEY], Value::Null);
        // Known PC, no display name → user set, display name null.
        assert_eq!(rows[1][ACCOUNT_USER_KEY], Value::String("WG\\kiosk".into()));
        assert_eq!(rows[1][ACCOUNT_DISPLAY_NAME_KEY], Value::Null);
    }

    #[test]
    fn scalar_columns_empty_when_all_fields_are_tables() {
        let hint = InventoryHint {
            display: vec![
                display("apps", Some("table")),
                display("disks", Some("table")),
            ],
            summary: None,
            explode: None,
            history_scalars: None,
        };
        assert!(scalar_columns(&hint).is_empty());
    }
}
