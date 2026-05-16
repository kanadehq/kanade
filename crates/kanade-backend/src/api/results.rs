use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct ResultRow {
    pub request_id: String,
    pub pc_id: String,
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Optional `status` filter on the results listing. `success` keeps
/// only `exit_code = 0`; `failure` keeps everything else. Anything
/// else (or omitted) returns the unfiltered listing.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum StatusFilter {
    Success,
    Failure,
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
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM deployment_results");
    let mut sep = " WHERE ";

    if let Some(pc) = params.pc_id.as_deref().filter(|s| !s.is_empty()) {
        qb.push(sep).push("pc_id = ").push_bind(pc.to_owned());
        sep = " AND ";
    }
    if let Some(status) = &params.status {
        let cmp = match status {
            StatusFilter::Success => "exit_code = 0",
            StatusFilter::Failure => "exit_code <> 0",
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

pub async fn detail(
    State(pool): State<SqlitePool>,
    Path(request_id): Path<String>,
) -> Result<Json<ResultRow>, StatusCode> {
    let row = sqlx::query("SELECT * FROM deployment_results WHERE request_id = ?")
        .bind(&request_id)
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
        request_id: r.try_get("request_id").unwrap_or_default(),
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        exit_code: r.try_get("exit_code").unwrap_or(0),
        stdout: r.try_get("stdout").unwrap_or_default(),
        stderr: r.try_get("stderr").unwrap_or_default(),
        started_at: r.try_get("started_at").ok(),
        finished_at: r.try_get("finished_at").ok(),
    }
}
