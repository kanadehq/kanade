use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use regex::Regex;
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
    /// Regex on `pc_id`. Plain text without metacharacters acts as
    /// substring search (`PC001` matches `PC0010` too — anchor with
    /// `^PC001$` for exact).
    pub pc_id: Option<String>,
    /// Regex on `job_id`. NULL `job_id` values are matched as the
    /// empty string, so they pass the filter only when the regex
    /// also matches `""` — a `^foo` filter therefore excludes NULL
    /// rows, while leaving the field unset (or empty) keeps them.
    pub job_id: Option<String>,
    /// Regex on `exec_id`. Same empty-string-on-NULL semantics as
    /// `job_id`.
    pub exec_id: Option<String>,
    /// Regex on `stdout` content. Match runs against the whole
    /// stdout buffer; multiline patterns (`(?m)`) work as expected.
    pub stdout: Option<String>,
    /// Regex on `stderr` content — same shape as `stdout`.
    pub stderr: Option<String>,
    pub status: Option<StatusFilter>,
    /// ISO-8601 lower bound on `recorded_at`. Anything strictly older is filtered out.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_limit() -> u32 {
    50
}

// Upper bound on the prefilter window when at least one regex filter
// is active. SQL narrows by `status` + `since` first; then we ORDER BY
// recorded_at DESC and scan up to MAX_FETCH rows in Rust, applying the
// compiled regexes and stopping once `limit` matches are collected.
// Pulling the prefilter into Rust is the same trick used by
// `api::audit::list` — sqlx 0.8 doesn't expose `create_scalar_function`
// so a native REGEXP UDF isn't on the table. 10k rows is comfortable
// even when `stdout` / `stderr` are kilobytes each (≈ tens of MB in
// the worst case); operators wanting more should narrow `since`.
const MAX_FETCH: i64 = 10_000;

fn compile(opt: Option<&str>) -> Result<Option<Regex>, (StatusCode, String)> {
    match opt.filter(|s| !s.is_empty()) {
        Some(s) => Regex::new(s)
            .map(Some)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid regex `{s}`: {e}"))),
        None => Ok(None),
    }
}

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<ResultRow>>, (StatusCode, String)> {
    let pc_re = compile(params.pc_id.as_deref())?;
    let job_re = compile(params.job_id.as_deref())?;
    let exec_re = compile(params.exec_id.as_deref())?;
    let stdout_re = compile(params.stdout.as_deref())?;
    let stderr_re = compile(params.stderr.as_deref())?;
    let has_regex = pc_re.is_some()
        || job_re.is_some()
        || exec_re.is_some()
        || stdout_re.is_some()
        || stderr_re.is_some();

    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM execution_results");
    let mut sep = " WHERE ";

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
        sep = " AND ";
    }
    let _ = sep;

    qb.push(" ORDER BY recorded_at DESC LIMIT ");
    let sql_limit = if has_regex {
        MAX_FETCH
    } else {
        params.limit as i64
    };
    qb.push_bind(sql_limit);

    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
        warn!(error = %e, "list results");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "list results failed".to_string(),
        )
    })?;

    // Fast path: no regex filters → SQL already applied the LIMIT, so
    // just hydrate the rows and return. Avoids the per-row `try_get`
    // dance + the manual capacity / break logic below.
    if !has_regex {
        return Ok(Json(rows.into_iter().map(row_to_result).collect()));
    }

    // Regex path: match against the raw column values first so a row
    // that's about to be dropped never pays for `String::from(stdout)`
    // (potentially kilobytes). Only winners get hydrated into a
    // `ResultRow`. Read the columns as `&str` borrows on the row —
    // for SQLite NULL surfaces as Err on `try_get`, which we collapse
    // to "" so the regex sees the documented empty-string semantics.
    let limit = params.limit as usize;
    let mut out: Vec<ResultRow> = Vec::with_capacity(limit.min(64));
    for r in rows {
        if let Some(re) = &pc_re
            && !re.is_match(r.try_get::<&str, _>("pc_id").unwrap_or(""))
        {
            continue;
        }
        if let Some(re) = &job_re
            && !re.is_match(r.try_get::<&str, _>("job_id").unwrap_or(""))
        {
            continue;
        }
        if let Some(re) = &exec_re
            && !re.is_match(r.try_get::<&str, _>("exec_id").unwrap_or(""))
        {
            continue;
        }
        if let Some(re) = &stdout_re
            && !re.is_match(r.try_get::<&str, _>("stdout").unwrap_or(""))
        {
            continue;
        }
        if let Some(re) = &stderr_re
            && !re.is_match(r.try_get::<&str, _>("stderr").unwrap_or(""))
        {
            continue;
        }
        out.push(row_to_result(r));
        if out.len() >= limit {
            break;
        }
    }
    Ok(Json(out))
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
