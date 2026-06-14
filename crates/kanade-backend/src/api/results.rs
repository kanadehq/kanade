use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use kanade_shared::subject;
use kanade_shared::wire::{JobTailReply, JobTailRequest};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tracing::warn;

use super::AppState;

/// A regex-match excerpt for the Activity listing. The matched run is
/// returned split from its surrounding context (`before` / `matched` /
/// `after`) so the SPA can wrap the hit in `<mark>` without juggling
/// byte offsets — JS string indexing is UTF-16, so handing it raw
/// regex byte offsets would mis-slice any multibyte output. The
/// `clipped_*` flags say where the context was trimmed so the UI can
/// render a leading / trailing ellipsis. Surfacing this lets operators
/// see *where* a `stdout` / `stderr` filter matched even when the hit
/// sits well past the collapsed preview cutoff.
#[derive(Serialize, Debug, PartialEq)]
pub struct MatchSnippet {
    pub before: String,
    pub matched: String,
    pub after: String,
    pub clipped_start: bool,
    pub clipped_end: bool,
}

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
    /// True when the `stdout` returned here is a server-clipped preview
    /// (the listing ships the first PREVIEW_CHARS chars, not the whole
    /// buffer — the full body is one detail fetch away). Always false on
    /// the detail endpoint, which returns the complete buffer.
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// First regex-match excerpt when a `stdout` / `stderr` filter is
    /// active and hit this row. None on the no-regex fast path and on
    /// the detail endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_match: Option<MatchSnippet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_match: Option<MatchSnippet>,
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
    /// ISO-8601 lower bound on `started_at`. Anything strictly older is
    /// filtered out. #399: was `recorded_at` (backend projection time),
    /// which answered "what did the backend record recently" instead of
    /// the operator's actual question "what *ran* recently" — and after
    /// a -WipeDb re-projection (#389) every replayed row's recorded_at
    /// collapses onto the replay instant, flooding the default 24h
    /// window with weeks-old runs. `started_at` matches the column the
    /// table displays and is immune to re-projection.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_limit() -> u32 {
    50
}

// Upper bound on the prefilter window when at least one regex filter
// is active. SQL narrows by `status` + `since` first; then we ORDER BY
// started_at DESC and scan up to MAX_FETCH rows in Rust, applying the
// compiled regexes and stopping once `limit` matches are collected.
// Pulling the prefilter into Rust is the same trick used by
// `api::audit::list` — sqlx 0.8 doesn't expose `create_scalar_function`
// so a native REGEXP UDF isn't on the table.
//
// #517: the prefilter SELECT excludes `stdout` / `stderr` unless a
// regex actually targets them — post-#227 each can be ~256 KB
// inline, so `SELECT *` over the window was a multi-GB worst case
// (and a single keystroke in the Activity pc_id filter triggered
// it). Metadata-only rows are a few hundred bytes, so 10k is a few
// MB. When an output regex IS present the blobs must be fetched to
// match against, so that path gets a smaller window; operators
// wanting more should narrow `since`.
const MAX_FETCH: i64 = 10_000;
const MAX_FETCH_WITH_OUTPUT: i64 = 1_000;

/// Prefilter projection for the no-output-regex path: everything
/// `row_to_result` reads except the two blob columns. MUST stay in
/// sync with `row_to_result` — a column read there but missing here
/// silently returns its zero-value on every metadata-regex call
/// (guarded by `metadata_regex_projection_matches_full_row`).
const META_COLUMNS: &str =
    "result_id, request_id, exec_id, pc_id, exit_code, started_at, finished_at, job_id, version";

/// SQLite's default bind-parameter ceiling is 999; the blob
/// re-hydration `IN (...)` chunks its ids well under it so an
/// operator-supplied large `limit` can't blow the query up.
const HYDRATE_CHUNK: usize = 500;

/// Chars of stdout/stderr preview the listing returns per row. The full
/// body is fetched lazily from the detail endpoint when the operator
/// clicks "show more", so the listing payload stays small even when a
/// fleet-wide fan-out fills the window with kilobyte-each outputs.
const PREVIEW_CHARS: usize = 200;

/// Context chars kept on each side of a regex match in the snippet
/// excerpt so the SPA can show *where* the pattern hit.
const SNIPPET_CONTEXT_CHARS: usize = 80;

/// Cap on the matched substring inside a snippet — a greedy pattern
/// (`.*`) can span the whole buffer, which would defeat the payload
/// trim, so clip it and flag the clip via `clipped_end`.
const SNIPPET_MATCH_CHARS: usize = 240;

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<ResultRow>>, (StatusCode, String)> {
    let pc_re = super::compile(params.pc_id.as_deref())?;
    let job_re = super::compile(params.job_id.as_deref())?;
    let exec_re = super::compile(params.exec_id.as_deref())?;
    let stdout_re = super::compile(params.stdout.as_deref())?;
    let stderr_re = super::compile(params.stderr.as_deref())?;
    let has_regex = pc_re.is_some()
        || job_re.is_some()
        || exec_re.is_some()
        || stdout_re.is_some()
        || stderr_re.is_some();
    // #517: only fetch the (potentially ~256 KB each) blob columns
    // into the prefilter window when a regex actually matches
    // against them; otherwise winners are re-fetched by id below.
    let needs_output = stdout_re.is_some() || stderr_re.is_some();

    let select = if has_regex && !needs_output {
        format!("SELECT {META_COLUMNS} FROM execution_results")
    } else {
        "SELECT * FROM execution_results".to_string()
    };
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(select);
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
        qb.push(sep).push("started_at >= ").push_bind(since);
        sep = " AND ";
    }
    let _ = sep;

    // #399: order by started_at (the displayed column) so the listing
    // is a run-time timeline, not a projection-time feed. result_id
    // breaks ties (broadcast fan-outs can share a start instant) so
    // pagination across refetches is deterministic. Served by
    // idx_execution_results_started_at.
    qb.push(" ORDER BY started_at DESC, result_id DESC LIMIT ");
    let sql_limit = if !has_regex {
        params.limit as i64
    } else if needs_output {
        MAX_FETCH_WITH_OUTPUT
    } else {
        MAX_FETCH
    };
    qb.push_bind(sql_limit);

    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
        warn!(error = %e, "list results");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "list results failed".to_string(),
        )
    })?;

    // Fast path: no regex filters → SQL already applied the LIMIT.
    // Hydrate each row with a *clipped* stdout/stderr preview (the full
    // body is one detail fetch away on "show more") so a 1000-row
    // fleet-wide window of kilobyte-each outputs doesn't ship megabytes
    // of stdio the table only ever shows PREVIEW_CHARS of. The DB read
    // is unchanged; only the wire payload shrinks.
    if !has_regex {
        let out: Vec<ResultRow> = rows
            .iter()
            .map(|r| {
                let stdout_raw: &str = r.try_get("stdout").unwrap_or("");
                let stderr_raw: &str = r.try_get("stderr").unwrap_or("");
                let (sp, st) = take_prefix(stdout_raw, PREVIEW_CHARS);
                let (ep, et) = take_prefix(stderr_raw, PREVIEW_CHARS);
                build_row(r, sp.to_string(), ep.to_string(), st, et, None, None)
            })
            .collect();
        return Ok(Json(out));
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
        // Row survived every filter. Re-borrow the raw columns to build
        // the match excerpt (around the *first* hit) and the clipped
        // preview — matching already ran against the full text above, so
        // clipping the wire payload here doesn't change which rows
        // surface, only how much stdio rides along.
        let stdout_raw: &str = r.try_get("stdout").unwrap_or("");
        let stderr_raw: &str = r.try_get("stderr").unwrap_or("");
        let stdout_match = stdout_re
            .as_ref()
            .and_then(|re| match_snippet(re, stdout_raw));
        let stderr_match = stderr_re
            .as_ref()
            .and_then(|re| match_snippet(re, stderr_raw));
        let (sp, st) = take_prefix(stdout_raw, PREVIEW_CHARS);
        let (ep, et) = take_prefix(stderr_raw, PREVIEW_CHARS);
        out.push(build_row(
            &r,
            sp.to_string(),
            ep.to_string(),
            st,
            et,
            stdout_match,
            stderr_match,
        ));
        if out.len() >= limit {
            break;
        }
    }

    // #517: the metadata-only prefilter left stdout/stderr behind —
    // re-fetch them for just the winning rows (≤ `limit`, so this is
    // a handful of point lookups by primary key). Chunked to stay
    // under SQLite's 999 bind-parameter ceiling for large limits.
    if !needs_output && !out.is_empty() {
        let mut by_id: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::with_capacity(out.len());
        for chunk in out.chunks(HYDRATE_CHUNK) {
            let mut qb: QueryBuilder<Sqlite> =
                QueryBuilder::new("SELECT result_id, stdout, stderr FROM execution_results");
            qb.push(" WHERE result_id IN (");
            {
                let mut sep = qb.separated(", ");
                for row in chunk {
                    sep.push_bind(row.result_id.clone());
                }
            }
            qb.push(")");
            let blob_rows = qb.build().fetch_all(&pool).await.map_err(|e| {
                warn!(error = %e, "hydrate result output");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "list results failed".to_string(),
                )
            })?;
            by_id.extend(blob_rows.into_iter().map(|r| {
                (
                    r.try_get("result_id").unwrap_or_default(),
                    (
                        r.try_get("stdout").unwrap_or_default(),
                        r.try_get("stderr").unwrap_or_default(),
                    ),
                )
            }));
        }
        for row in &mut out {
            if let Some((stdout, stderr)) = by_id.remove(&row.result_id) {
                // Clip the re-fetched blobs to the preview, same as the
                // fast path — otherwise rehydration would smuggle the
                // full buffer back into the payload this PR set out to
                // shrink. No match excerpt here: this path runs only
                // when no stdout/stderr regex is active.
                let (sp, st) = take_prefix(&stdout, PREVIEW_CHARS);
                let (ep, et) = take_prefix(&stderr, PREVIEW_CHARS);
                row.stdout = sp.to_string();
                row.stderr = ep.to_string();
                row.stdout_truncated = st;
                row.stderr_truncated = et;
            }
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

/// Live-tail response for `GET /api/results/{result_id}/tail`.
///
/// Three shapes, distinguished by `live` / `running`:
/// - **finished** (`running = false`, `live = false`): the row has a
///   `finished_at`, so the persisted stdout/stderr is the whole truth.
///   The SPA stops polling and shows the final output + exit code.
/// - **live** (`live = true`): the addressed agent answered with a
///   ring-buffer snapshot of the still-(or just-)running job. `stdout`
///   / `stderr` are the tail (a suffix when `*_truncated`).
/// - **waiting** (`running = true`, `live = false`): the row is
///   in-flight but the agent has no live buffer to serve (offline, or
///   the run is on an agent that pre-dates this feature, or it timed
///   out). The SPA keeps polling and shows a "waiting for output" hint.
#[derive(Serialize)]
pub struct TailResponse {
    pub running: bool,
    pub live: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub exit_code: Option<i64>,
}

/// `GET /api/results/{result_id}/tail` — live stdout/stderr for the
/// SPA's "live" toggle. Mirrors the `agent_logs::tail` request/reply
/// pattern: a finished row is served straight from the DB; an in-flight
/// row triggers a `job.tail.<pc_id>` round-trip to the live agent.
pub async fn tail(
    State(state): State<AppState>,
    Path(result_id): Path<String>,
) -> Result<Json<TailResponse>, StatusCode> {
    let row = sqlx::query(
        "SELECT pc_id, finished_at, exit_code, stdout, stderr \
         FROM execution_results WHERE result_id = ?",
    )
    .bind(&result_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "tail: lookup result row");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    let pc_id: String = row.try_get("pc_id").unwrap_or_default();
    let finished_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("finished_at").ok();
    let exit_code: Option<i64> = row.try_get("exit_code").ok();

    // Finished + projected: the DB row is authoritative. No NATS hop.
    if finished_at.is_some() {
        return Ok(Json(TailResponse {
            running: false,
            live: false,
            stdout: row.try_get("stdout").unwrap_or_default(),
            stderr: row.try_get("stderr").unwrap_or_default(),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code,
        }));
    }

    // In-flight: ask the agent for its live ring buffer. A timeout or
    // a `found = false` reply both degrade to the "waiting" shape — the
    // SPA keeps polling rather than erroring out.
    let waiting = || {
        Ok(Json(TailResponse {
            running: true,
            live: false,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: None,
        }))
    };

    let req = JobTailRequest {
        result_id: result_id.clone(),
    };
    let payload = match serde_json::to_vec(&req) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "tail: encode JobTailRequest");
            return waiting();
        }
    };
    let subject = subject::job_tail(&pc_id);
    // 3s, deliberately under the SPA's 5s poll interval: the agent
    // answers a live-tail request in single-digit ms when online, so a
    // longer wait only matters when it's unreachable. Failing fast
    // returns the `waiting` shape before the next poll fires, so slow /
    // offline agents can't pile up overlapping in-flight requests on
    // the backend.
    let reply = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        state.nats.request(subject, payload.into()),
    )
    .await
    {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => {
            warn!(error = %e, %pc_id, "tail: job.tail request failed");
            return waiting();
        }
        Err(_) => {
            // Agent didn't reply in time — keep the SPA polling.
            return waiting();
        }
    };

    let parsed: JobTailReply = match serde_json::from_slice(&reply.payload) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "tail: decode JobTailReply");
            return waiting();
        }
    };

    if !parsed.found {
        // Agent has no live buffer for this id (evicted past grace, or
        // never ran here). Fall back to "waiting"; the next poll will
        // likely find a finished row once the ExecResult projects.
        return waiting();
    }

    Ok(Json(TailResponse {
        running: parsed.running,
        live: true,
        stdout: parsed.stdout,
        stderr: parsed.stderr,
        stdout_truncated: parsed.stdout_truncated,
        stderr_truncated: parsed.stderr_truncated,
        exit_code: None,
    }))
}

/// First `n` chars of `s` plus whether `s` had more (so the caller can
/// flag truncation / render an ellipsis). Operates on char boundaries
/// so a multibyte sequence (CP932-origin U+FFFD, emoji, …) is never
/// split mid-codepoint.
fn take_prefix(s: &str, n: usize) -> (&str, bool) {
    match s.char_indices().nth(n) {
        Some((idx, _)) => (&s[..idx], true),
        None => (s, false),
    }
}

/// Last `n` chars of `s` plus whether `s` had more — the suffix mirror
/// of `take_prefix`, used for the leading context of a match excerpt.
/// Walks from the end so it's O(n) in the context window rather than
/// O(L) in the (potentially multi-MB) buffer length.
fn take_suffix(s: &str, n: usize) -> (&str, bool) {
    if n == 0 {
        return ("", !s.is_empty());
    }
    let mut iter = s.char_indices().rev();
    match iter.nth(n - 1) {
        // `nth(n-1)` lands on the n-th char from the end (its byte
        // index); a further `next()` tells us whether an (n+1)-th char
        // exists, i.e. whether the head was clipped.
        Some((idx, _)) => (&s[idx..], iter.next().is_some()),
        None => (s, false),
    }
}

/// Build a match excerpt around the *first* hit of `re` in `hay`,
/// splitting the matched run from its surrounding context so the SPA
/// can wrap it in `<mark>`. Returns None when `re` doesn't match (e.g.
/// the row qualified on a different filter axis like `pc_id`).
fn match_snippet(re: &Regex, hay: &str) -> Option<MatchSnippet> {
    let m = re.find(hay)?;
    let (before, clipped_start) = take_suffix(&hay[..m.start()], SNIPPET_CONTEXT_CHARS);
    let (matched, matched_clipped) = take_prefix(&hay[m.start()..m.end()], SNIPPET_MATCH_CHARS);
    let (after, after_clipped) = take_prefix(&hay[m.end()..], SNIPPET_CONTEXT_CHARS);
    Some(MatchSnippet {
        before: before.to_string(),
        matched: matched.to_string(),
        after: after.to_string(),
        clipped_start,
        clipped_end: matched_clipped || after_clipped,
    })
}

/// Assemble a `ResultRow` from a DB row plus the already-derived
/// stdout/stderr payload — the full body on the detail endpoint, a
/// clipped preview (+ optional match excerpt) on the listing.
fn build_row(
    r: &sqlx::sqlite::SqliteRow,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    stdout_match: Option<MatchSnippet>,
    stderr_match: Option<MatchSnippet>,
) -> ResultRow {
    ResultRow {
        result_id: r.try_get("result_id").unwrap_or_default(),
        request_id: r.try_get("request_id").unwrap_or_default(),
        exec_id: r.try_get("exec_id").ok(),
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        // v0.30 / PR α' unified: exit_code is now NULLABLE.
        // try_get(...).ok() collapses absent + NULL to None — the
        // SPA renders that as "—" / running placeholder.
        exit_code: r.try_get("exit_code").ok(),
        stdout,
        stderr,
        started_at: r.try_get("started_at").ok(),
        finished_at: r.try_get("finished_at").ok(),
        // try_get → ok() collapses both "column missing entirely"
        // (legacy DB pre-migration 0002) and "column NULL" (ad-hoc
        // `kanade run` rows) to None, which is what we want.
        job_id: r.try_get("job_id").ok(),
        version: r.try_get("version").ok(),
        stdout_truncated,
        stderr_truncated,
        stdout_match,
        stderr_match,
    }
}

/// Detail endpoint hydration: the whole buffer, never clipped, no match
/// excerpt (the detail view shows the full body verbatim).
fn row_to_result(r: sqlx::sqlite::SqliteRow) -> ResultRow {
    let stdout: String = r.try_get("stdout").unwrap_or_default();
    let stderr: String = r.try_get("stderr").unwrap_or_default();
    build_row(&r, stdout, stderr, false, false, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn params(since: Option<chrono::DateTime<chrono::Utc>>) -> ListParams {
        ListParams {
            limit: default_limit(),
            pc_id: None,
            job_id: None,
            exec_id: None,
            stdout: None,
            stderr: None,
            status: None,
            since,
        }
    }

    /// Insert a finished row the way the results projector does:
    /// every timestamp — including `recorded_at` — bound through
    /// chrono (RFC 3339), never left to the column DEFAULT.
    async fn insert_row(pool: &SqlitePool, result_id: &str) {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at)
             VALUES (?, 'req', 'pc-1', 0, '', '', ?, ?, ?)",
        )
        .bind(result_id)
        .bind(now - Duration::minutes(10))
        .bind(now - Duration::minutes(9))
        .bind(now - Duration::minutes(9))
        .execute(pool)
        .await
        .unwrap();
    }

    /// #390 regression (filter axis updated to `started_at` by #399):
    /// a row that ran minutes ago must match a rolling `since` bound
    /// from the same UTC day. The #390 failure mode was a timestamp
    /// TEXT-format mismatch (DEFAULT CURRENT_TIMESTAMP's space
    /// separator vs RFC 3339's 'T'; ' ' < 'T' lexicographically)
    /// that pushed every same-UTC-date row below the bound, so the
    /// Activity page's "last 24h" showed nothing until the next UTC
    /// midnight (= 09:00 JST).
    #[tokio::test]
    async fn since_filter_matches_same_utc_day_rows() {
        let pool = fresh_pool().await;
        insert_row(&pool, "r-1").await;

        let rows = list(
            State(pool.clone()),
            Query(params(Some(Utc::now() - Duration::hours(24)))),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(
            rows.len(),
            1,
            "row that ran minutes ago must be inside the rolling 24h window",
        );

        // Sanity: a bound minutes in the future excludes it.
        let rows = list(
            State(pool),
            Query(params(Some(Utc::now() + Duration::minutes(5)))),
        )
        .await
        .unwrap()
        .0;
        assert!(rows.is_empty(), "future bound must exclude the row");
    }

    /// #399: the filter axis is `started_at` (what the table shows),
    /// NOT `recorded_at` (projection time). A row projected just now
    /// but started three weeks ago — exactly what a -WipeDb
    /// re-projection (#389) produces — must NOT pass a 24h `since`.
    #[tokio::test]
    async fn since_filters_on_started_at_not_recorded_at() {
        let pool = fresh_pool().await;
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at)
             VALUES ('r-replayed', 'req', 'pc-1', 0, '', '', ?, ?, ?)",
        )
        .bind(now - Duration::days(21)) // ran three weeks ago...
        .bind(now - Duration::days(21))
        .bind(now) // ...but (re-)projected just now
        .execute(&pool)
        .await
        .unwrap();

        let rows = list(
            State(pool.clone()),
            Query(params(Some(now - Duration::hours(24)))),
        )
        .await
        .unwrap()
        .0;
        assert!(
            rows.is_empty(),
            "a re-projected three-week-old run must not flood the 24h window",
        );

        // It still shows up once the window actually covers its run time.
        let rows = list(State(pool), Query(params(Some(now - Duration::days(30)))))
            .await
            .unwrap()
            .0;
        assert_eq!(rows.len(), 1);
    }

    /// #517: a metadata regex (pc_id) uses the blob-free prefilter,
    /// then re-hydrates stdout/stderr for the winners — the response
    /// must still carry the (clipped) output.
    #[tokio::test]
    async fn metadata_regex_path_still_returns_output() {
        let pool = fresh_pool().await;
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at)
             VALUES ('r-out', 'req', 'pc-9', 0, 'hello stdout', 'hello stderr', ?, ?, ?)",
        )
        .bind(now - Duration::minutes(10))
        .bind(now - Duration::minutes(9))
        .bind(now - Duration::minutes(9))
        .execute(&pool)
        .await
        .unwrap();
        insert_row(&pool, "r-other").await; // pc-1, must not match

        let mut p = params(None);
        p.pc_id = Some("^pc-9$".into());
        let rows = list(State(pool), Query(p)).await.unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_id, "r-out");
        assert_eq!(
            rows[0].stdout, "hello stdout",
            "winners must be re-hydrated with their output",
        );
        assert_eq!(rows[0].stderr, "hello stderr");
    }

    /// #517 schema-drift guard: a row served through the metadata-
    /// regex path must be field-for-field identical to the same row
    /// served by the fast path. If `row_to_result` grows a column
    /// read that `META_COLUMNS` is missing, the regex-path copy
    /// silently carries the zero-value and this comparison fails.
    #[tokio::test]
    async fn metadata_regex_projection_matches_full_row() {
        let pool = fresh_pool().await;
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, exec_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at, job_id, version)
             VALUES ('r-full', 'req-1', 'ex-1', 'pc-7', 3, 'out body', 'err body',
                     ?, ?, ?, 'job-x', '1.2.3')",
        )
        .bind(now - Duration::minutes(10))
        .bind(now - Duration::minutes(9))
        .bind(now - Duration::minutes(9))
        .execute(&pool)
        .await
        .unwrap();

        let fast = list(State(pool.clone()), Query(params(None)))
            .await
            .unwrap()
            .0;
        let mut p = params(None);
        p.pc_id = Some("pc-7".into());
        let via_regex = list(State(pool), Query(p)).await.unwrap().0;

        assert_eq!(fast.len(), 1);
        assert_eq!(via_regex.len(), 1);
        assert_eq!(
            serde_json::to_value(&fast[0]).unwrap(),
            serde_json::to_value(&via_regex[0]).unwrap(),
            "metadata projection + hydration must reproduce the full row exactly",
        );
    }

    /// #517: an output regex needs the blobs in the prefilter window
    /// itself — that path must keep matching against stdout.
    #[tokio::test]
    async fn stdout_regex_path_matches_against_output() {
        let pool = fresh_pool().await;
        let now = Utc::now();
        for (id, out) in [("r-hit", "ERROR: kaboom"), ("r-miss", "all fine")] {
            sqlx::query(
                "INSERT INTO execution_results
                    (result_id, request_id, pc_id, exit_code, stdout, stderr,
                     started_at, finished_at, recorded_at)
                 VALUES (?, 'req', 'pc-1', 0, ?, '', ?, ?, ?)",
            )
            .bind(id)
            .bind(out)
            .bind(now - Duration::minutes(10))
            .bind(now - Duration::minutes(9))
            .bind(now - Duration::minutes(9))
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut p = params(None);
        p.stdout = Some("(?i)error".into());
        let rows = list(State(pool), Query(p)).await.unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_id, "r-hit");
        assert_eq!(rows[0].stdout, "ERROR: kaboom");
    }

    /// Like `insert_row` but lets the test pick the stdout/stderr bodies
    /// so the preview-clip + match-excerpt paths can be exercised.
    async fn insert_row_io(pool: &SqlitePool, result_id: &str, stdout: &str, stderr: &str) {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at)
             VALUES (?, 'req', 'pc-1', 0, ?, ?, ?, ?, ?)",
        )
        .bind(result_id)
        .bind(stdout)
        .bind(stderr)
        .bind(now - Duration::minutes(10))
        .bind(now - Duration::minutes(9))
        .bind(now - Duration::minutes(9))
        .execute(pool)
        .await
        .unwrap();
    }

    /// The listing must clip stdout to a preview (and flag it) so a
    /// fleet-wide window doesn't ship the full buffer of every row —
    /// the full body is fetched lazily from the detail endpoint.
    #[tokio::test]
    async fn list_clips_stdout_to_preview() {
        let pool = fresh_pool().await;
        let long = "x".repeat(PREVIEW_CHARS + 50);
        insert_row_io(&pool, "r-long", &long, "").await;

        let rows = list(
            State(pool),
            Query(params(Some(Utc::now() - Duration::hours(24)))),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.stdout.chars().count(),
            PREVIEW_CHARS,
            "stdout must be clipped to the preview length",
        );
        assert!(row.stdout_truncated, "a clipped stdout must be flagged");
        assert!(!row.stderr_truncated, "empty stderr is not truncated");
        assert!(
            row.stdout_match.is_none(),
            "no regex filter → no match excerpt",
        );
    }

    /// #517 + preview: a metadata-regex winner whose blobs are
    /// re-hydrated must still be clipped to the preview (the rehydration
    /// must not smuggle the full buffer back into the payload).
    #[tokio::test]
    async fn metadata_regex_path_clips_rehydrated_output() {
        let pool = fresh_pool().await;
        let long = "y".repeat(PREVIEW_CHARS + 80);
        // pc-9 so a `^pc-9$` metadata regex selects it via the
        // blob-free prefilter, forcing the rehydration path.
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at)
             VALUES ('r-big', 'req', 'pc-9', 0, ?, '', ?, ?, ?)",
        )
        .bind(&long)
        .bind(now - Duration::minutes(10))
        .bind(now - Duration::minutes(9))
        .bind(now - Duration::minutes(9))
        .execute(&pool)
        .await
        .unwrap();

        let mut p = params(None);
        p.pc_id = Some("^pc-9$".into());
        let rows = list(State(pool), Query(p)).await.unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].stdout.chars().count(),
            PREVIEW_CHARS,
            "rehydrated output must still be clipped to the preview",
        );
        assert!(
            rows[0].stdout_truncated,
            "a clipped rehydrated stdout must be flagged",
        );
    }

    /// A stdout regex hit must come back with a match excerpt even when
    /// the needle sits well past the preview cutoff — that's the only
    /// way the SPA can show operators *where* it matched.
    #[tokio::test]
    async fn stdout_regex_returns_match_excerpt() {
        let pool = fresh_pool().await;
        let hay = format!("{}NEEDLE{}", "a".repeat(PREVIEW_CHARS + 20), "b".repeat(30));
        insert_row_io(&pool, "r-match", &hay, "").await;

        let mut p = params(Some(Utc::now() - Duration::hours(24)));
        p.stdout = Some("NEEDLE".to_string());
        let rows = list(State(pool), Query(p)).await.unwrap().0;
        assert_eq!(rows.len(), 1);
        let m = rows[0]
            .stdout_match
            .as_ref()
            .expect("a stdout match must carry an excerpt");
        assert_eq!(m.matched, "NEEDLE");
        assert!(m.clipped_start, "context before the needle was clipped");
        assert!(m.before.ends_with('a'), "before-context is the lead-in");
        assert!(m.after.starts_with('b'), "after-context is the tail");
    }

    /// A greedy pattern can match the whole buffer; the excerpt must cap
    /// the matched run so it can't defeat the payload trim.
    #[tokio::test]
    async fn greedy_match_is_capped() {
        let pool = fresh_pool().await;
        let hay = "z".repeat(SNIPPET_MATCH_CHARS + 100);
        insert_row_io(&pool, "r-greedy", &hay, "").await;

        let mut p = params(Some(Utc::now() - Duration::hours(24)));
        p.stdout = Some("z+".to_string());
        let rows = list(State(pool), Query(p)).await.unwrap().0;
        let m = rows[0].stdout_match.as_ref().unwrap();
        assert_eq!(
            m.matched.chars().count(),
            SNIPPET_MATCH_CHARS,
            "matched run is capped",
        );
        assert!(m.clipped_end, "a capped match flags clipped_end");
    }
}
