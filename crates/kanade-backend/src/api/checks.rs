//! Compliance read API (#290 PR-E). `GET /api/checks` returns the
//! fleet-wide `check_status` rows the SPA Compliance page renders —
//! which PCs pass / warn / fail / unknown for each operator-defined
//! `check:` job (those with `fleet` enabled; `fleet: false` checks stay
//! client-only and never project here). One row per (pc_id, check),
//! latest status — not a time series.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::warn;

use super::AppState;

#[derive(Serialize, sqlx::FromRow)]
pub struct CheckRow {
    pub pc_id: String,
    pub check_name: String,
    /// `ok` / `warn` / `fail` / `unknown` (normalised by the projector).
    pub status: String,
    pub detail: Option<String>,
    /// `NOT NULL` in the schema, so a required field — a decode failure
    /// surfaces as a 500 rather than being silently masked to `None`.
    pub recorded_at: DateTime<Utc>,
}

/// `GET /api/checks` — every projected compliance row, ordered by check
/// then PC. The SPA groups them into the fleet matrix (check × PC).
pub async fn list_all(
    State(state): State<AppState>,
) -> Result<Json<Vec<CheckRow>>, (StatusCode, String)> {
    // `query_as` propagates real sqlx decode errors (type mismatch,
    // missing column) instead of the `try_get(...).ok()` idiom that
    // silently defaults them away.
    let rows: Vec<CheckRow> = sqlx::query_as(
        "SELECT pc_id, check_name, status, detail, recorded_at
         FROM check_status
         ORDER BY check_name, pc_id",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "check_status query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    Ok(Json(rows))
}
