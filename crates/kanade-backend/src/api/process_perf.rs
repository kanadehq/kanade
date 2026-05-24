//! `/api/agents/{pc_id}/processes` — most recent per-process snapshot.
//!
//! V1 surface is intentionally minimal: returns the top-N processes
//! from the **latest** tick the projector has on file for this PC.
//! No time-range slicing yet — the operator use case is "what's
//! pegging this host right now?", so a snapshot is more useful than
//! a timeline at this stage. Per-PID timelines are a SPA-side
//! follow-up once we have data to chart.
//!
//! When no samples exist yet (process_perf was never enabled, or the
//! 7-day retention has aged them all out), the response carries an
//! empty `processes` array and `latest_at = null` — the SPA renders
//! that as "no samples in window yet" instead of a 404, because the
//! PC itself is still a valid agent.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct ProcessesResponse {
    pub pc_id: String,
    pub latest_at: Option<DateTime<Utc>>,
    pub processes: Vec<ProcessRow>,
}

#[derive(Serialize)]
pub struct ProcessRow {
    pub pid: i64,
    pub name: String,
    pub cpu_pct: f64,
    pub rss_bytes: i64,
    pub disk_read_bytes_per_sec: Option<f64>,
    pub disk_written_bytes_per_sec: Option<f64>,
}

pub async fn processes(
    State(pool): State<SqlitePool>,
    Path(pc_id): Path<String>,
) -> Result<Json<ProcessesResponse>, StatusCode> {
    // Two-step: pin the latest `at` first, then SELECT the row set
    // belonging to that tick. SQLite's planner handles this just
    // fine without a CTE, and it makes the empty-table branch (no
    // samples yet) easy to detect without parsing back a NULL `at`.
    let latest_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(at) FROM process_perf_samples WHERE pc_id = ?")
            .bind(&pc_id)
            .fetch_one(&pool)
            .await
            .map_err(|e| {
                warn!(error = %e, pc_id, "process_perf latest_at query");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    let Some(at) = latest_at else {
        return Ok(Json(ProcessesResponse {
            pc_id,
            latest_at: None,
            processes: vec![],
        }));
    };

    let rows = sqlx::query(
        "SELECT pid, name, cpu_pct, rss_bytes,
                disk_read_bytes_per_sec, disk_written_bytes_per_sec
         FROM process_perf_samples
         WHERE pc_id = ? AND at = ?
         ORDER BY cpu_pct DESC",
    )
    .bind(&pc_id)
    .bind(at)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id, "process_perf rows query");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let processes = rows
        .into_iter()
        .map(|r| ProcessRow {
            pid: r.try_get("pid").unwrap_or(0),
            name: r.try_get("name").unwrap_or_default(),
            cpu_pct: r.try_get("cpu_pct").unwrap_or(0.0),
            rss_bytes: r.try_get("rss_bytes").unwrap_or(0),
            disk_read_bytes_per_sec: r.try_get("disk_read_bytes_per_sec").ok(),
            disk_written_bytes_per_sec: r.try_get("disk_written_bytes_per_sec").ok(),
        })
        .collect();

    Ok(Json(ProcessesResponse {
        pc_id,
        latest_at: Some(at),
        processes,
    }))
}
