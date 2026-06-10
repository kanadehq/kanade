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
use kanade_shared::manifest::{DisplayField, ExplodeSpec, Manifest};
use serde::Serialize;
use sqlx::{AssertSqlSafe, Row};
use tracing::warn;

use crate::projector::explode::validate_ident;

use super::AppState;

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

    let rows = sqlx::query(
        "SELECT pc_id, facts_json, display_json, summary_json, collected_at
         FROM inventory_facts
         WHERE job_id = ?
           AND (?2 IS NULL OR pc_id LIKE ?2 ESCAPE '\\')
         ORDER BY pc_id
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
            });
        }
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
    Ok(Json(out))
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
    let hint = manifest.inventory.ok_or((
        StatusCode::NOT_FOUND,
        format!("manifest {manifest_id:?} has no inventory hint"),
    ))?;
    let specs = hint.explode.ok_or((
        StatusCode::NOT_FOUND,
        format!("manifest {manifest_id:?} has no explode specs"),
    ))?;
    // Populate the cache before answering — subsequent requests
    // for any field on this manifest go straight through. Empty
    // hint.explode would have errored above; here `specs` always
    // has at least one entry.
    state
        .explode_spec_cache
        .insert(manifest_id.to_string(), specs.clone())
        .await;
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
}
