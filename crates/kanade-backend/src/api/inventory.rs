//! Inventory facts read API. Three shapes:
//!
//!   * `GET /api/inventory/<pc_id>` — every probe's facts for one
//!     PC. Drives the SPA's detail view (vertical field/value).
//!   * `GET /api/inventory/by-job/<manifest_id>` — one probe's facts
//!     across every PC that's reported it. Drives the SPA's fleet
//!     list (row per PC, columns = summary fields).
//!   * `GET /api/inventory/jobs` — fleet-wide listing of inventory-
//!     tagged manifests, with both display + summary configs inline.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use kanade_shared::kv::BUCKET_JOBS;
use kanade_shared::manifest::{DisplayField, Manifest};
use serde::Serialize;
use sqlx::Row;
use tracing::warn;

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
