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
use sqlx::Row;
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
}

pub async fn list_for_job(
    State(state): State<AppState>,
    Path(manifest_id): Path<String>,
) -> Result<Json<InventoryByJob>, (StatusCode, String)> {
    let rows = sqlx::query(
        "SELECT pc_id, facts_json, display_json, summary_json, collected_at
         FROM inventory_facts
         WHERE job_id = ?
         ORDER BY pc_id",
    )
    .bind(&manifest_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, %manifest_id, "inventory_facts by-job query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Pull display + summary from the first row that has them;
    // every row for one manifest_id has the same snapshot, since
    // the projector writes them together at upsert.
    let display = rows
        .iter()
        .find_map(|r| {
            r.try_get::<Option<String>, _>("display_json")
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<Vec<DisplayField>>(&s).ok())
        })
        .unwrap_or_default();
    let summary = rows.iter().find_map(|r| {
        r.try_get::<Option<String>, _>("summary_json")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Vec<DisplayField>>(&s).ok())
    });

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

    let mut q = sqlx::query(&sql);
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
async fn load_explode_spec(
    state: &AppState,
    manifest_id: &str,
    field: &str,
) -> Result<ExplodeSpec, (StatusCode, String)> {
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
    specs.into_iter().find(|s| s.field == field).ok_or((
        StatusCode::NOT_FOUND,
        format!("manifest {manifest_id:?} has no explode field {field:?}"),
    ))
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
