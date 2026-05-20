use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct ResultRow {
    /// v0.29 / Issue #19: PK (agent-minted per-PC UUID). For rows
    /// projected by pre-v0.29 agents the migration backfilled
    /// `result_id = request_id`, so old links keep resolving.
    pub result_id: String,
    pub request_id: String,
    /// v0.29 / Issue #19: back-link to `executions.exec_id`. `None`
    /// for ad-hoc `kanade run` rows and for rows that pre-date the
    /// migration.
    pub exec_id: Option<String>,
    pub pc_id: String,
    /// v0.30 / PR α' unified: NULL while the run is in-flight (the
    /// row was created by events.started, ExecResult hasn't landed
    /// yet). The SPA renders "—" / running placeholder for None.
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// v0.30 / PR α' unified: NULL means the run is still in
    /// flight. Once the matching ExecResult lands the results
    /// projector UPSERTs and sets this to the script's finish
    /// timestamp. Combined with `exit_code` this is the unified
    /// "running" vs "finished" signal.
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// v0.27: surface `execution_results.job_id` (column added in
    /// migration 0002) so the SPA Results page can route operators
    /// to `POST /api/jobs/{job_id}/kill` with a single click. None
    /// when the row pre-dates migration 0002 or when the result
    /// arrived via an ad-hoc `kanade run` (no Job behind it).
    pub job_id: Option<String>,
    /// v0.30 / PR α' unified: pinned Manifest version, populated by
    /// the events.started insert (events payload carries
    /// Command.version). None for legacy rows + result-first rows
    /// (no events.started landed) — the Activity Finished view
    /// falls back to "—".
    pub version: Option<String>,
}

/// Optional `status` filter on the results listing. `success` keeps
/// only `exit_code = 0`; `failure` keeps everything else.
/// `running` selects in-flight rows (events.started landed but no
/// ExecResult yet, so finished_at IS NULL). Anything else (or
/// omitted) returns the unfiltered listing.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum StatusFilter {
    Success,
    Failure,
    /// v0.30 / PR α' unified: in-flight rows = events.started landed
    /// but no matching ExecResult yet. Activity Running view filter.
    Running,
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub pc_id: Option<String>,
    pub status: Option<StatusFilter>,
    /// ISO-8601 lower bound on `recorded_at`. Anything strictly older is filtered out.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_limit() -> u32 {
    50
}

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<ResultRow>>, StatusCode> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM execution_results");
    let mut sep = " WHERE ";

    if let Some(pc) = params.pc_id.as_deref().filter(|s| !s.is_empty()) {
        qb.push(sep).push("pc_id = ").push_bind(pc.to_owned());
        sep = " AND ";
    }
    if let Some(status) = &params.status {
        let cmp = match status {
            // Both success + failure require the run to be finished
            // (exit_code IS NOT NULL ⇒ finished_at IS NOT NULL).
            // Pre-v0.30 schemas implicitly had exit_code NOT NULL so
            // adding the explicit check is back-compatible.
            StatusFilter::Success => "exit_code = 0",
            StatusFilter::Failure => "exit_code IS NOT NULL AND exit_code <> 0",
            // v0.30 / PR α' unified: in-flight rows = finished_at
            // not set yet. Activity Running tab filters via this.
            StatusFilter::Running => "finished_at IS NULL",
        };
        qb.push(sep).push(cmp);
        sep = " AND ";
    }
    if let Some(since) = params.since {
        qb.push(sep).push("recorded_at >= ").push_bind(since);
        let _ = sep;
    }

    qb.push(" ORDER BY recorded_at DESC LIMIT ")
        .push_bind(params.limit as i64);

    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
        warn!(error = %e, "list results");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(rows.into_iter().map(row_to_result).collect()))
}

/// `GET /api/results/{id}` — `{id}` is now `result_id` (v0.29).
/// Pre-v0.29 rows had `result_id == request_id` after the migration
/// backfill, so legacy links from cached browser tabs still resolve.
/// Brand-new rows from broadcast Commands each have their own
/// `result_id` so the SPA can finally show per-PC results that
/// previously got de-duped to one row.
pub async fn detail(
    State(pool): State<SqlitePool>,
    Path(id): Path<String>,
) -> Result<Json<ResultRow>, StatusCode> {
    let row = sqlx::query("SELECT * FROM execution_results WHERE result_id = ?")
        .bind(&id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "detail result");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    match row {
        Some(r) => Ok(Json(row_to_result(r))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

fn row_to_result(r: sqlx::sqlite::SqliteRow) -> ResultRow {
    ResultRow {
        result_id: r.try_get("result_id").unwrap_or_default(),
        request_id: r.try_get("request_id").unwrap_or_default(),
        exec_id: r.try_get("exec_id").ok(),
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        // v0.30 / PR α' unified: exit_code is now NULLABLE.
        // try_get(...).ok() collapses absent + NULL to None — the
        // SPA renders that as "—" / running placeholder.
        exit_code: r.try_get("exit_code").ok(),
        stdout: r.try_get("stdout").unwrap_or_default(),
        stderr: r.try_get("stderr").unwrap_or_default(),
        started_at: r.try_get("started_at").ok(),
        finished_at: r.try_get("finished_at").ok(),
        // try_get → ok() collapses both "column missing entirely"
        // (legacy DB pre-migration 0002) and "column NULL" (ad-hoc
        // `kanade run` rows) to None, which is what we want.
        job_id: r.try_get("job_id").ok(),
        version: r.try_get("version").ok(),
    }
}
