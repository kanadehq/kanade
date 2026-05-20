//! `GET /api/runs` — per-(exec_id, pc_id) in-flight rows from the
//! `running_runs` table populated by the v0.30 events projector.
//! Drives the SPA Activity Running tab (PR β follow-up).
//!
//! Returns rows where `finished_at IS NULL` — these are PCs that
//! emitted `events.started` but haven't published an ExecResult yet.
//! Once the result lands, the results projector UPDATEs
//! `finished_at` and the row drops out of this listing.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct RunningRow {
    pub exec_id: String,
    pub pc_id: String,
    /// Gemini #72 medium fix: tightened from `Option<DateTime>` to
    /// non-optional since `running_runs.started_at TIMESTAMP NOT NULL`
    /// at the schema level. The SPA Running tab can render it
    /// directly without an `??` fallback.
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub manifest_id: String,
    pub version: String,
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Optional filter on `exec_id` — narrows to one deployment.
    /// Useful for the Activity Running tab's per-exec drill-down.
    pub exec_id: Option<String>,
}

fn default_limit() -> u32 {
    200
}

/// `GET /api/runs?limit=N&exec_id=X` — list in-flight runs. Filter
/// param `exec_id` is optional; without it the response is the
/// full fleet's in-flight set (newest first).
pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<RunningRow>>, StatusCode> {
    // QueryBuilder so the optional `exec_id` filter slots in via
    // `push_bind` rather than string-concat — same pattern as
    // results::list / executions::list.
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT exec_id, pc_id, started_at, manifest_id, version \
           FROM running_runs \
          WHERE finished_at IS NULL",
    );
    if let Some(eid) = params.exec_id.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND exec_id = ").push_bind(eid.to_owned());
    }
    qb.push(" ORDER BY started_at DESC LIMIT ")
        .push_bind(params.limit as i64);

    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
        warn!(error = %e, "list running runs");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(rows.into_iter().map(row_to_running).collect()))
}

fn row_to_running(r: sqlx::sqlite::SqliteRow) -> RunningRow {
    RunningRow {
        exec_id: r.try_get("exec_id").unwrap_or_default(),
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        // Schema declares started_at NOT NULL. `expect` here means
        // a coercion failure (= schema/code drift) is loud and
        // fixable, not silently papered over with `Utc::now()`
        // which would corrupt the Running view's ordering.
        started_at: r
            .try_get("started_at")
            .expect("running_runs.started_at is NOT NULL per migration 0004"),
        manifest_id: r.try_get("manifest_id").unwrap_or_default(),
        version: r.try_get("version").unwrap_or_default(),
    }
}
