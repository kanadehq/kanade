//! `GET /api/inventory/<pc_id>` — per-PC inventory facts.
//!
//! Returns one entry per manifest_id that has produced inventory
//! facts for this PC. Each entry carries the raw facts JSON (whatever
//! the operator's PowerShell probe emitted) plus the display config
//! the manifest declared, so the SPA can render columns without
//! reaching back into the schedules KV.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use kanade_shared::kv::BUCKET_SCHEDULES;
use kanade_shared::manifest::{DisplayField, Schedule};
use serde::Serialize;
use sqlx::Row;
use tracing::warn;

use super::AppState;

#[derive(Serialize)]
pub struct InventoryFact {
    pub job_id: String,
    pub facts: serde_json::Value,
    pub display: Vec<DisplayField>,
    pub collected_at: Option<DateTime<Utc>>,
    pub recorded_at: Option<DateTime<Utc>>,
}

pub async fn list_for_pc(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<Json<Vec<InventoryFact>>, (StatusCode, String)> {
    let rows = sqlx::query(
        "SELECT job_id, facts_json, display_json, collected_at, recorded_at
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

/// `GET /api/inventory/jobs` — list every inventory-tagged schedule
/// in the fleet (one row per manifest.id that has an `inventory:`
/// hint). The SPA Inventory page uses this to render a list of probes
/// even before any PC has reported facts.
#[derive(Serialize)]
pub struct InventoryJob {
    pub manifest_id: String,
    pub description: Option<String>,
    pub display: Vec<DisplayField>,
}

pub async fn list_jobs(
    State(state): State<AppState>,
) -> Result<Json<Vec<InventoryJob>>, (StatusCode, String)> {
    let kv = state
        .jetstream
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("get KV {BUCKET_SCHEDULES}: {e}"),
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
        let schedule: Schedule = match serde_json::from_slice(&entry) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Some(hint) = schedule.manifest.inventory {
            out.push(InventoryJob {
                manifest_id: schedule.manifest.id,
                description: schedule.manifest.description,
                display: hint.display,
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
    InventoryFact {
        job_id: r.try_get("job_id").unwrap_or_default(),
        facts,
        display,
        collected_at: r.try_get("collected_at").ok(),
        recorded_at: r.try_get("recorded_at").ok(),
    }
}
