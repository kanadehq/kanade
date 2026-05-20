//! `GET /api/executions` and `GET /api/executions/{exec_id}` — surfaces
//! the `executions` table. Added in v0.29 / Issue #19 once the results
//! projector started incrementing `success_count` / `failure_count`
//! per result instead of leaving them at 0.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct ExecutionRow {
    pub exec_id: String,
    /// `Manifest.id` of the job that was fired (= `jobs.{id}`).
    pub job_id: String,
    pub version: String,
    pub initiated_by: String,
    pub initiated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub target_count: i64,
    pub success_count: i64,
    pub failure_count: i64,
    /// pending → running → completed. Maintained by the results
    /// projector's CASE-driven UPDATE (v0.29 / Issue #19).
    pub status: String,
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Optional filter on `executions.job_id` (the cmd id, not the
    /// exec_id) — lets the SPA show only fires of one job.
    pub job_id: Option<String>,
    /// Optional filter on `executions.status` ('pending' / 'running'
    /// / 'completed'). No validation — unknown values just match
    /// nothing.
    pub status: Option<String>,
}

fn default_limit() -> u32 {
    50
}

/// `GET /api/executions` — list, newest-first. The 'pending' status
/// rows show ones that haven't received any results yet; 'running'
/// rows are partially complete; 'completed' rows have all
/// `target_count` results in.
pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<ExecutionRow>>, StatusCode> {
    // Explicit column list (not `SELECT *`) — keeps the projection
    // stable across future `executions` schema additions and makes
    // the row-decoder contract obvious.
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT exec_id, job_id, version, initiated_by, initiated_at, \
                target_count, success_count, failure_count, status \
           FROM executions",
    );
    let mut sep = " WHERE ";
    if let Some(jid) = params.job_id.as_deref().filter(|s| !s.is_empty()) {
        qb.push(sep).push("job_id = ").push_bind(jid.to_owned());
        sep = " AND ";
    }
    if let Some(st) = params.status.as_deref().filter(|s| !s.is_empty()) {
        qb.push(sep).push("status = ").push_bind(st.to_owned());
        let _ = sep;
    }
    qb.push(" ORDER BY initiated_at DESC LIMIT ")
        .push_bind(params.limit as i64);

    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
        warn!(error = %e, "list executions");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(rows.into_iter().map(row_to_execution).collect()))
}

/// `GET /api/executions/{exec_id}` — one deployment + its per-PC
/// ExecResult rows. The SPA's upcoming Activity Running tab uses this
/// to show "exec X fired against N PCs, here's who reported back".
pub async fn detail(
    State(pool): State<SqlitePool>,
    Path(exec_id): Path<String>,
) -> Result<Json<ExecutionDetail>, StatusCode> {
    let row = sqlx::query(
        "SELECT exec_id, job_id, version, initiated_by, initiated_at, \
                target_count, success_count, failure_count, status \
           FROM executions WHERE exec_id = ?",
    )
    .bind(&exec_id)
    .fetch_optional(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "detail execution");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let execution = match row {
        Some(r) => row_to_execution(r),
        None => return Err(StatusCode::NOT_FOUND),
    };
    let result_rows = sqlx::query(
        "SELECT result_id, request_id, pc_id, exit_code, started_at,
                finished_at, recorded_at
           FROM execution_results
          WHERE exec_id = ?
          ORDER BY recorded_at DESC",
    )
    .bind(&exec_id)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "detail execution results");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let results: Vec<ExecutionResultSummary> = result_rows
        .into_iter()
        .map(|r| ExecutionResultSummary {
            result_id: r.try_get("result_id").unwrap_or_default(),
            request_id: r.try_get("request_id").unwrap_or_default(),
            pc_id: r.try_get("pc_id").unwrap_or_default(),
            exit_code: r.try_get("exit_code").unwrap_or(0),
            started_at: r.try_get("started_at").ok(),
            finished_at: r.try_get("finished_at").ok(),
        })
        .collect();
    Ok(Json(ExecutionDetail { execution, results }))
}

#[derive(Serialize)]
pub struct ExecutionDetail {
    pub execution: ExecutionRow,
    pub results: Vec<ExecutionResultSummary>,
}

/// Trimmed projection of `execution_results` — stdout / stderr are
/// dropped from the list so a big fan-out's detail payload doesn't
/// balloon. The Activity detail page fetches per-result full payloads
/// via the existing `GET /api/results/{result_id}` on demand.
#[derive(Serialize)]
pub struct ExecutionResultSummary {
    pub result_id: String,
    pub request_id: String,
    pub pc_id: String,
    pub exit_code: i64,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn row_to_execution(r: sqlx::sqlite::SqliteRow) -> ExecutionRow {
    ExecutionRow {
        exec_id: r.try_get("exec_id").unwrap_or_default(),
        job_id: r.try_get("job_id").unwrap_or_default(),
        version: r.try_get("version").unwrap_or_default(),
        initiated_by: r.try_get("initiated_by").unwrap_or_default(),
        initiated_at: r.try_get("initiated_at").ok(),
        target_count: r.try_get("target_count").unwrap_or(0),
        success_count: r.try_get("success_count").unwrap_or(0),
        failure_count: r.try_get("failure_count").unwrap_or(0),
        status: r.try_get("status").unwrap_or_default(),
    }
}
