use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use kanade_shared::subject;
use kanade_shared::wire::{JobTailReply, JobTailRequest};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tracing::warn;

use super::AppState;
use super::time_bounds::{bound_in_range, bounds_in_range};

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
    /// #955: for a `<job>__finalize` row, the `result_id` of the run
    /// whose `finalize:` hook produced it. `None` for every ordinary
    /// run. Lets the SPA link a finalize row back to its parent run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_result_id: Option<String>,
    pub pc_id: String,
    /// v0.30 / PR α' unified: NULL while the run is in-flight (the
    /// row was created by events.started, ExecResult hasn't landed
    /// yet). The SPA renders "—" / running placeholder for None.
    pub exit_code: Option<i64>,
    /// The agent published this row instead of running the script
    /// (`ExecResult::skipped`); `exit_code` then holds the reserved code
    /// saying why. Always false for rows from agents that predate the
    /// flag. The SPA badges on this, never on the exit code.
    pub skipped: bool,
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
    /// #955: the `finalize:` hook rows this run spawned (the reverse of
    /// `parent_result_id`). Populated ONLY by the detail endpoint — the
    /// listing leaves it empty (skipped on the wire) to avoid an N+1
    /// query per row. Lets the SPA link a run forward to its finalize
    /// row(s).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub finalize_children: Vec<FinalizeChild>,
}

/// #955: a compact reference to a `<job>__finalize` result row, returned
/// on the parent run's detail so the SPA can render a "→ finalize" link
/// without a second fetch.
#[derive(Serialize)]
pub struct FinalizeChild {
    pub result_id: String,
    pub job_id: Option<String>,
    pub exit_code: Option<i64>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Optional `status` filter on the results listing. `success` keeps
/// only `exit_code = 0`; `skipped` keeps the rows the agent published
/// instead of running the script (`execution_results.skipped`); `failure`
/// keeps every other finished row.
/// `running` selects in-flight rows (events.started landed but no
/// ExecResult yet, so finished_at IS NULL). Anything else (or
/// omitted) returns the unfiltered listing.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusFilter {
    Success,
    Failure,
    Skipped,
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
    /// ISO-8601 inclusive lower bound on `started_at`. Anything strictly
    /// older is filtered out. #399: was `recorded_at` (backend projection
    /// time), which answered "what did the backend record recently"
    /// instead of the operator's actual question "what *ran* recently" —
    /// and after a -WipeDb re-projection (#389) every replayed row's
    /// recorded_at collapses onto the replay instant, flooding the default
    /// 24h window with weeks-old runs. `started_at` matches the column the
    /// table displays and is immune to re-projection.
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// ISO-8601 inclusive upper bound on `started_at` (#1666). Pairs with
    /// `since` to bound the time window from above; older pages within the
    /// same search series advance via `cursor`.
    #[serde(default)]
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    /// Opaque continuation token returned in `X-Results-Next-Cursor`
    /// (#1666). Resumes strictly after the last inspected `(started_at,
    /// result_id)` pair in descending order and verifies that the active
    /// filter parameters have not changed since the cursor was issued.
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_limit() -> u32 {
    50
}

/// Hard ceiling on `limit` (#1666). Matches the SPA's largest page-size
/// selector (`1000`) and bounds per-response hydration + wire payload.
const MAX_LIMIT: u32 = 1_000;

/// Per-request scan budget when at least one non-exact regex filter is
/// evaluated in Rust (#1666).
///
/// Exact anchored literal filters on `pc_id` / `job_id` / `exec_id`
/// (`^<literal>$` with no flags or regex operators) are pushed down to
/// SQL equality and use the composite `(col, started_at DESC, result_id
/// DESC)` indexes directly. General regexes (unanchored substring
/// searches, alternations, flags, character classes, or `stdout` /
/// `stderr` content patterns) are evaluated in Rust in bounded chunks up
/// to `MAX_FETCH` rows per request (`MAX_FETCH_WITH_OUTPUT` when an
/// output regex requires reading the ~256 KB stdio columns).
///
/// When a request reaches `limit` or exhausts its scan budget while older
/// rows remain in the SQL-filtered range, the response sets
/// `X-Results-Stop-Reason` (`limit_reached` or `scan_limit_reached`) and
/// `X-Results-Next-Cursor` at the last *inspected* row so the caller can
/// continue scanning older history without skipping uninspected rows or
/// mistaking an exhausted scan window for "no matches".
const MAX_FETCH: usize = 10_000;
const MAX_FETCH_WITH_OUTPUT: usize = 1_000;

/// Chunk size per SQL fetch during a Rust-regex scan (#1666). Fetching in
/// chunks avoids pulling all 10,000 metadata rows (or 1,000 stdio blob
/// rows) when `limit` matches are found near the top of the window.
const SCAN_CHUNK: usize = 1_000;
const SCAN_CHUNK_WITH_OUTPUT: usize = 250;

/// Maximum encoded length and per-field length accepted for `cursor`.
const MAX_CURSOR_LEN: usize = 512;
const MAX_CURSOR_FIELD_LEN: usize = 256;
const CURSOR_VERSION: u8 = 1;

/// Response header reporting how many candidate rows were inspected in
/// this request.
pub const HEADER_SCANNED: &str = "x-results-scanned";
/// Response header reporting why scanning stopped: `limit_reached`,
/// `scan_limit_reached`, or `end_of_range`.
pub const HEADER_STOP_REASON: &str = "x-results-stop-reason";
/// Response header carrying the opaque cursor to resume scanning older
/// rows when `X-Results-Stop-Reason` is not `end_of_range`.
pub const HEADER_NEXT_CURSOR: &str = "x-results-next-cursor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    LimitReached,
    ScanLimitReached,
    EndOfRange,
}

impl StopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LimitReached => "limit_reached",
            Self::ScanLimitReached => "scan_limit_reached",
            Self::EndOfRange => "end_of_range",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct ScanCursor {
    v: u8,
    started_at: String,
    result_id: String,
    filter_hash: String,
}

/// Prefilter projection for the no-output-regex path: everything
/// `row_to_result` reads except the two blob columns. MUST stay in
/// sync with `row_to_result` — a column read there but missing here
/// silently returns its zero-value on every metadata-regex call
/// (guarded by `metadata_regex_projection_matches_full_row`).
const META_COLUMNS: &str = "result_id, request_id, exec_id, parent_result_id, pc_id, exit_code, skipped, started_at, finished_at, job_id, version";

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

/// Extract an exact literal when `raw` (after trimming, matching
/// [`super::compile`]) is an anchored `^...$` regex whose body contains
/// only literal characters and escaped regex punctuation, with no flags,
/// character classes, quantifiers, groups, or alternation, and whose
/// decoded literal is non-empty.
///
/// Returns `None` for any pattern whose semantics are not strictly
/// identical to SQLite `=` (`BINARY` collation):
/// - Unanchored literals (`PC001`) are substring searches (`PC001` matches
///   `PC0010`) and must stay on the Rust `Regex` path.
/// - Empty literals (`^$`) match `NULL` on nullable columns (`job_id`,
///   `exec_id`) because the Rust path folds `NULL` to `""`, whereas SQL
///   `col = ''` rejects `NULL`.
/// - Inline flags (`(?i)`), character classes (`\d`, `[a-z]`), alternation
///   (`^a|b$`), and assertions (`\b`, `\A`, `\z`) stay on the Rust path.
fn extract_exact_literal(raw: Option<&str>) -> Option<String> {
    let s = raw.map(str::trim).filter(|s| !s.is_empty())?;
    let body = s.strip_prefix('^')?.strip_suffix('$')?;
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // If `chars.next()` is `None`, the trailing `$` in `s` was
                // escaped (`\$`), so the pattern has no end anchor.
                let escaped = chars.next()?;
                if matches!(
                    escaped,
                    '.' | '*'
                        | '+'
                        | '?'
                        | '^'
                        | '$'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '|'
                        | '\\'
                        | '/'
                        | '-'
                ) {
                    out.push(escaped);
                } else {
                    return None;
                }
            }
            '.' | '*' | '+' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\r'
            | '\n' => {
                return None;
            }
            _ => out.push(c),
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

fn norm_opt(opt: &Option<String>) -> Option<&str> {
    opt.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn compute_filter_hash(params: &ListParams) -> String {
    use sha2::{Digest, Sha256};

    let status_str = match params.status {
        Some(StatusFilter::Success) => "success",
        Some(StatusFilter::Failure) => "failure",
        Some(StatusFilter::Skipped) => "skipped",
        Some(StatusFilter::Running) => "running",
        None => "",
    };
    let since_str = params
        .since
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_default();
    let until_str = params
        .until
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_default();

    let canonical = serde_json::json!([
        norm_opt(&params.pc_id),
        norm_opt(&params.job_id),
        norm_opt(&params.exec_id),
        norm_opt(&params.stdout),
        norm_opt(&params.stderr),
        status_str,
        since_str,
        until_str,
    ]);
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn encode_cursor(started_at: &str, result_id: &str, filter_hash: &str) -> Option<String> {
    if started_at.is_empty() || result_id.is_empty() {
        return None;
    }
    let payload = ScanCursor {
        v: CURSOR_VERSION,
        started_at: started_at.to_string(),
        result_id: result_id.to_string(),
        filter_hash: filter_hash.to_string(),
    };
    let json = serde_json::to_vec(&payload).ok()?;
    use base64::Engine;
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
}

fn decode_cursor(
    raw: &str,
    expected_filter_hash: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(String, String), (StatusCode, String)> {
    use base64::Engine;

    if raw.is_empty() || raw.len() > MAX_CURSOR_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            "cursor: invalid length".to_string(),
        ));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "cursor: invalid base64 encoding".to_string(),
            )
        })?;
    let parsed: ScanCursor = serde_json::from_slice(&bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "cursor: invalid payload".to_string(),
        )
    })?;
    if parsed.v != CURSOR_VERSION {
        return Err((
            StatusCode::BAD_REQUEST,
            "cursor: unsupported version".to_string(),
        ));
    }
    if parsed.started_at.is_empty()
        || parsed.started_at.len() > MAX_CURSOR_FIELD_LEN
        || parsed.result_id.is_empty()
        || parsed.result_id.len() > MAX_CURSOR_FIELD_LEN
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "cursor: invalid key fields".to_string(),
        ));
    }
    let cursor_dt = parsed
        .started_at
        .parse::<chrono::DateTime<chrono::Utc>>()
        .or_else(|_| {
            // SQLite の旧形式も受け入れ、比較には元の文字列を保持する。
            chrono::NaiveDateTime::parse_from_str(&parsed.started_at, "%Y-%m-%d %H:%M:%S%.f")
                .map(|dt| dt.and_utc())
        })
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "cursor: invalid started_at timestamp".to_string(),
            )
        })?;
    if !bound_in_range(cursor_dt) {
        return Err((
            StatusCode::BAD_REQUEST,
            "cursor: started_at year out of range (must be 0..=9999)".to_string(),
        ));
    }
    if parsed.filter_hash != expected_filter_hash {
        return Err((
            StatusCode::BAD_REQUEST,
            "cursor: does not match active search filters".to_string(),
        ));
    }
    if let Some(s) = since
        && cursor_dt < s
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "cursor: started_at is older than since".to_string(),
        ));
    }
    if let Some(u) = until
        && cursor_dt > u
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "cursor: started_at is newer than until".to_string(),
        ));
    }
    // Keep the exact stored `started_at` string for SQLite's lexical
    // comparison rather than reformatting `cursor_dt`.
    Ok((parsed.started_at, parsed.result_id))
}

struct SqlFilters<'a> {
    status: Option<StatusFilter>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    pc_exact: Option<&'a str>,
    job_exact: Option<&'a str>,
    exec_exact: Option<&'a str>,
}

fn push_sql_filters(
    qb: &mut QueryBuilder<Sqlite>,
    filters: &SqlFilters<'_>,
    after: Option<&(String, String)>,
) {
    let mut sep = " WHERE ";
    if let Some(status) = filters.status {
        qb.push(sep);
        match status {
            // Success / failure / skipped all require the run to be
            // finished (exit_code IS NOT NULL ⇒ finished_at IS NOT NULL).
            // Pre-v0.30 schemas implicitly had exit_code NOT NULL so
            // adding the explicit check is back-compatible.
            StatusFilter::Success => qb.push("exit_code = 0"),
            StatusFilter::Failure => {
                qb.push("exit_code IS NOT NULL AND exit_code <> 0 AND skipped = 0")
            }
            StatusFilter::Skipped => qb.push("skipped = 1"),
            // v0.30 / PR α' unified: in-flight rows = finished_at
            // not set yet. Activity Running tab filters via this.
            StatusFilter::Running => qb.push("finished_at IS NULL"),
        };
        sep = " AND ";
    }
    if let Some(pc_id) = filters.pc_exact {
        qb.push(sep).push("pc_id = ").push_bind(pc_id.to_owned());
        sep = " AND ";
    }
    if let Some(job_id) = filters.job_exact {
        qb.push(sep).push("job_id = ").push_bind(job_id.to_owned());
        sep = " AND ";
    }
    if let Some(exec_id) = filters.exec_exact {
        qb.push(sep)
            .push("exec_id = ")
            .push_bind(exec_id.to_owned());
        sep = " AND ";
    }
    if let Some(since) = filters.since {
        qb.push(sep).push("started_at >= ").push_bind(since);
        sep = " AND ";
    }
    if let Some(until) = filters.until {
        qb.push(sep).push("started_at <= ").push_bind(until);
        sep = " AND ";
    }
    if let Some((started_at, result_id)) = after {
        qb.push(sep)
            .push("(started_at, result_id) < (")
            .push_bind(started_at.clone())
            .push(", ")
            .push_bind(result_id.clone())
            .push(")");
        sep = " AND ";
    }
    let _ = sep;
}

fn search_query(
    select: &str,
    filters: &SqlFilters<'_>,
    after: Option<&(String, String)>,
    limit: i64,
) -> QueryBuilder<Sqlite> {
    let mut qb = QueryBuilder::new(select.to_owned());
    push_sql_filters(&mut qb, filters, after);
    qb.push(" ORDER BY started_at DESC, result_id DESC LIMIT ")
        .push_bind(limit);
    qb
}

async fn has_older_rows(
    pool: &SqlitePool,
    filters: &SqlFilters<'_>,
    after: &(String, String),
) -> Result<bool, (StatusCode, String)> {
    let mut qb = search_query("SELECT 1 FROM execution_results", filters, Some(after), 1);
    let row = qb.build().fetch_optional(pool).await.map_err(|e| {
        warn!(error = %e, "list results lookahead");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "list results failed".to_string(),
        )
    })?;
    Ok(row.is_some())
}

fn build_response_headers(
    scanned: usize,
    stop_reason: StopReason,
    next_cursor: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&scanned.to_string()) {
        headers.insert(HEADER_SCANNED, v);
    }
    headers.insert(
        HEADER_STOP_REASON,
        HeaderValue::from_static(stop_reason.as_str()),
    );
    if let Some(cursor) = next_cursor
        && let Ok(v) = HeaderValue::from_str(cursor)
    {
        headers.insert(HEADER_NEXT_CURSOR, v);
    }
    headers
}

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<(HeaderMap, Json<Vec<ResultRow>>), (StatusCode, String)> {
    if params.limit == 0 || params.limit > MAX_LIMIT {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("limit must be between 1 and {MAX_LIMIT}"),
        ));
    }
    if !bounds_in_range([params.since]) {
        return Err((
            StatusCode::BAD_REQUEST,
            "since: year out of range (must be 0..=9999)".to_string(),
        ));
    }
    if !bounds_in_range([params.until]) {
        return Err((
            StatusCode::BAD_REQUEST,
            "until: year out of range (must be 0..=9999)".to_string(),
        ));
    }
    if let (Some(since), Some(until)) = (params.since, params.until)
        && since > until
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "since must not be after until".to_string(),
        ));
    }

    let mut pc_re = super::compile(params.pc_id.as_deref())?;
    let mut job_re = super::compile(params.job_id.as_deref())?;
    let mut exec_re = super::compile(params.exec_id.as_deref())?;
    let stdout_re = super::compile(params.stdout.as_deref())?;
    let stderr_re = super::compile(params.stderr.as_deref())?;

    // #1666: push strictly exact, flag-free non-empty literal regexes on
    // identifier columns down to SQL equality so indexed lookups don't
    // waste the 10k/1k scan window on unrelated rows. General regexes
    // (including unanchored substring literals and `^$`) stay in Rust.
    let pc_exact = extract_exact_literal(params.pc_id.as_deref());
    if pc_exact.is_some() {
        pc_re = None;
    }
    let job_exact = extract_exact_literal(params.job_id.as_deref());
    if job_exact.is_some() {
        job_re = None;
    }
    let exec_exact = extract_exact_literal(params.exec_id.as_deref());
    if exec_exact.is_some() {
        exec_re = None;
    }

    let filter_hash = compute_filter_hash(&params);
    let initial_cursor = match params.cursor.as_deref() {
        Some(raw) => Some(decode_cursor(
            raw,
            &filter_hash,
            params.since,
            params.until,
        )?),
        None => None,
    };

    let has_rust_regex = pc_re.is_some()
        || job_re.is_some()
        || exec_re.is_some()
        || stdout_re.is_some()
        || stderr_re.is_some();
    // #517: only fetch the (potentially ~256 KB each) blob columns
    // into the prefilter window when a regex actually matches
    // against them; otherwise winners are re-fetched by id below.
    let needs_output = stdout_re.is_some() || stderr_re.is_some();

    let sql_filters = SqlFilters {
        status: params.status,
        since: params.since,
        until: params.until,
        pc_exact: pc_exact.as_deref(),
        job_exact: job_exact.as_deref(),
        exec_exact: exec_exact.as_deref(),
    };
    let limit = params.limit as usize;

    // Fast path: no Rust-evaluated regex filters remain (either no regex
    // was supplied or every regex was an exact identifier literal pushed
    // down to SQL). Fetch `limit + 1` rows so a single query both fills
    // the page and determines whether older rows remain.
    if !has_rust_regex {
        let mut qb = search_query(
            "SELECT * FROM execution_results",
            &sql_filters,
            initial_cursor.as_ref(),
            (limit + 1) as i64,
        );
        // #399: order by started_at (the displayed column) so the listing
        // is a run-time timeline, not a projection-time feed. result_id
        // breaks ties (broadcast fan-outs can share a start instant) so
        // pagination across refetches is deterministic.

        let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
            warn!(error = %e, "list results");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "list results failed".to_string(),
            )
        })?;

        let has_more = rows.len() > limit;
        let mut last_inspected: Option<(String, String)> = None;
        let out: Vec<ResultRow> = rows
            .iter()
            .take(limit)
            .map(|r| {
                last_inspected = Some((
                    r.try_get::<String, _>("started_at").unwrap_or_default(),
                    r.try_get::<String, _>("result_id").unwrap_or_default(),
                ));
                let stdout_raw: &str = r.try_get("stdout").unwrap_or("");
                let stderr_raw: &str = r.try_get("stderr").unwrap_or("");
                let (sp, st) = take_prefix(stdout_raw, PREVIEW_CHARS);
                let (ep, et) = take_prefix(stderr_raw, PREVIEW_CHARS);
                build_row(r, sp.to_string(), ep.to_string(), st, et, None, None)
            })
            .collect();

        let scanned = out.len();
        let (stop_reason, next_cursor) = if has_more {
            let cur = last_inspected
                .as_ref()
                .and_then(|(s, id)| encode_cursor(s, id, &filter_hash));
            (StopReason::LimitReached, cur)
        } else {
            (StopReason::EndOfRange, None)
        };
        let headers = build_response_headers(scanned, stop_reason, next_cursor.as_deref());
        return Ok((headers, Json(out)));
    }

    // Regex scan path: fetch candidate rows in bounded chunks up to the
    // per-request scan budget (`MAX_FETCH` / `MAX_FETCH_WITH_OUTPUT`) and
    // match against the raw `&str` column borrows so non-matching rows
    // never allocate `String` copies.
    let scan_budget = if needs_output {
        MAX_FETCH_WITH_OUTPUT
    } else {
        MAX_FETCH
    };
    let chunk_cap = if needs_output {
        SCAN_CHUNK_WITH_OUTPUT
    } else {
        SCAN_CHUNK
    };
    let select = if needs_output {
        "SELECT * FROM execution_results".to_string()
    } else {
        format!("SELECT {META_COLUMNS} FROM execution_results")
    };

    let mut out: Vec<ResultRow> = Vec::with_capacity(limit.min(64));
    let mut scanned: usize = 0;
    let mut last_inspected: Option<(String, String)> = None;
    let mut exhausted_sql = false;
    let mut has_uninspected_fetched_row = false;

    while out.len() < limit && scanned < scan_budget {
        let fetch_count = (scan_budget - scanned).min(chunk_cap);
        let after = last_inspected.as_ref().or(initial_cursor.as_ref());

        let mut qb = search_query(&select, &sql_filters, after, fetch_count as i64);

        let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
            warn!(error = %e, "list results");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "list results failed".to_string(),
            )
        })?;

        let fetched_len = rows.len();
        if fetched_len == 0 {
            exhausted_sql = true;
            break;
        }

        for (idx, r) in rows.into_iter().enumerate() {
            scanned += 1;
            last_inspected = Some((
                r.try_get::<String, _>("started_at").unwrap_or_default(),
                r.try_get::<String, _>("result_id").unwrap_or_default(),
            ));

            let matched = pc_re
                .as_ref()
                .is_none_or(|re| re.is_match(r.try_get::<&str, _>("pc_id").unwrap_or("")))
                && job_re
                    .as_ref()
                    .is_none_or(|re| re.is_match(r.try_get::<&str, _>("job_id").unwrap_or("")))
                && exec_re
                    .as_ref()
                    .is_none_or(|re| re.is_match(r.try_get::<&str, _>("exec_id").unwrap_or("")))
                && stdout_re
                    .as_ref()
                    .is_none_or(|re| re.is_match(r.try_get::<&str, _>("stdout").unwrap_or("")))
                && stderr_re
                    .as_ref()
                    .is_none_or(|re| re.is_match(r.try_get::<&str, _>("stderr").unwrap_or("")));

            if matched {
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
            }

            if out.len() >= limit {
                if idx + 1 < fetched_len {
                    has_uninspected_fetched_row = true;
                } else if fetched_len < fetch_count {
                    exhausted_sql = true;
                }
                break;
            }
        }

        if out.len() >= limit {
            break;
        }
        if fetched_len < fetch_count {
            exhausted_sql = true;
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

    let has_more = if exhausted_sql {
        false
    } else if has_uninspected_fetched_row {
        true
    } else if let Some(ref after) = last_inspected {
        has_older_rows(&pool, &sql_filters, after).await?
    } else {
        false
    };

    let (stop_reason, next_cursor) = if !has_more {
        (StopReason::EndOfRange, None)
    } else {
        let cur = last_inspected
            .as_ref()
            .and_then(|(s, id)| encode_cursor(s, id, &filter_hash));
        if out.len() >= limit {
            (StopReason::LimitReached, cur)
        } else {
            (StopReason::ScanLimitReached, cur)
        }
    };

    let headers = build_response_headers(scanned, stop_reason, next_cursor.as_deref());
    Ok((headers, Json(out)))
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
    let Some(r) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    let mut out = row_to_result(r);

    // #955: hydrate the reverse `finalize:` link — the `<job>__finalize`
    // row(s) this run spawned (a run normally has 0 or 1). Served by the
    // partial index `idx_execution_results_parent_result_id`. A failure
    // here is non-fatal: the run detail still renders, just without the
    // forward link.
    match sqlx::query(
        "SELECT result_id, job_id, exit_code, finished_at \
         FROM execution_results WHERE parent_result_id = ? \
         ORDER BY started_at ASC",
    )
    .bind(&out.result_id)
    .fetch_all(&pool)
    .await
    {
        Ok(children) => {
            out.finalize_children = children
                .iter()
                .map(|c| FinalizeChild {
                    result_id: c.try_get("result_id").unwrap_or_default(),
                    job_id: c.try_get::<Option<String>, _>("job_id").unwrap_or(None),
                    exit_code: c.try_get::<Option<i64>, _>("exit_code").unwrap_or(None),
                    finished_at: c
                        .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at")
                        .unwrap_or(None),
                })
                .collect();
        }
        Err(e) => warn!(error = %e, result_id = %out.result_id, "detail: finalize children lookup"),
    }

    Ok(Json(out))
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
    // Explicit `Option<_>` reads: `try_get("exit_code").ok()` would infer
    // `try_get::<i64>` and sqlx-sqlite decodes NULL → `Some(0)`, so an
    // in-flight row (NULL exit_code) would tail as "exit 0" instead of
    // None/running. (#590)
    let finished_at: Option<chrono::DateTime<chrono::Utc>> = row
        .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at")
        .unwrap_or(None);
    let exit_code: Option<i64> = row.try_get::<Option<i64>, _>("exit_code").unwrap_or(None);

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
        // Nullable columns are read as `Option<_>` EXPLICITLY. sqlx-sqlite
        // decodes a NULL via `try_get::<i64>` / `<String>` into `0` / `""`
        // rather than erroring, so the old `try_get(..).ok()` form yielded
        // `Some(0)` / `Some("")` for NULL — mislabelling an in-flight row
        // (NULL exit_code) as "exit 0 / success" and an ad-hoc-run row
        // (NULL exec_id/job_id) as `Some("")`. The `Option<_>` form maps
        // NULL → None; `.unwrap_or(None)` still folds an absent column
        // (legacy DB pre-migration 0002) to None. (#590; same fix as #589's
        // schedule_run_stats.)
        exec_id: r.try_get::<Option<String>, _>("exec_id").unwrap_or(None),
        // #955: NULL for ordinary rows and for a DB that pre-dates the
        // migration (`unwrap_or(None)` folds the absent column to None).
        parent_result_id: r
            .try_get::<Option<String>, _>("parent_result_id")
            .unwrap_or(None),
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        exit_code: r.try_get::<Option<i64>, _>("exit_code").unwrap_or(None),
        skipped: r.try_get("skipped").unwrap_or(false),
        stdout,
        stderr,
        started_at: r
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at")
            .unwrap_or(None),
        finished_at: r
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at")
            .unwrap_or(None),
        job_id: r.try_get::<Option<String>, _>("job_id").unwrap_or(None),
        version: r.try_get::<Option<String>, _>("version").unwrap_or(None),
        stdout_truncated,
        stderr_truncated,
        stdout_match,
        stderr_match,
        // #955: filled only by the detail endpoint (see `detail`); the
        // listing leaves it empty so it's skipped on the wire.
        finalize_children: Vec::new(),
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
            until: None,
            cursor: None,
        }
    }

    async fn list_rows(pool: SqlitePool, p: ListParams) -> Vec<ResultRow> {
        list(State(pool), Query(p)).await.unwrap().1.0
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

    /// #955: a `<job>__finalize` row carries `parent_result_id`; the
    /// detail endpoint links the two directions — the parent run surfaces
    /// its finalize child, and the finalize row surfaces its parent.
    #[tokio::test]
    async fn detail_links_run_and_finalize_bidirectionally() {
        let pool = fresh_pool().await;
        insert_row(&pool, "parent-1").await;
        // A finalize row: exec_id NULL, its own job_id, and
        // parent_result_id pointing at the run it cleaned up.
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at, job_id, parent_result_id)
             VALUES ('fin-1', 'req__finalize', 'pc-1', 1, '', 'Remove-Item: denied',
                     ?, ?, ?, 'screenshot-collect__finalize', 'parent-1')",
        )
        .bind(now - Duration::minutes(8))
        .bind(now - Duration::minutes(7))
        .bind(now - Duration::minutes(7))
        .execute(&pool)
        .await
        .unwrap();

        // Parent detail → forward link to the finalize child.
        let parent = detail(State(pool.clone()), Path("parent-1".into()))
            .await
            .unwrap()
            .0;
        assert_eq!(parent.parent_result_id, None);
        assert_eq!(parent.finalize_children.len(), 1);
        assert_eq!(parent.finalize_children[0].result_id, "fin-1");
        assert_eq!(
            parent.finalize_children[0].job_id.as_deref(),
            Some("screenshot-collect__finalize"),
        );
        assert_eq!(parent.finalize_children[0].exit_code, Some(1));

        // Finalize detail → back-link to the parent, and no children of
        // its own.
        let fin = detail(State(pool), Path("fin-1".into())).await.unwrap().0;
        assert_eq!(fin.parent_result_id.as_deref(), Some("parent-1"));
        assert!(fin.finalize_children.is_empty());
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

        let rows = list_rows(pool.clone(), params(Some(Utc::now() - Duration::hours(24)))).await;
        assert_eq!(
            rows.len(),
            1,
            "row that ran minutes ago must be inside the rolling 24h window",
        );

        // Sanity: a bound minutes in the future excludes it.
        let rows = list_rows(pool, params(Some(Utc::now() + Duration::minutes(5)))).await;
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

        let rows = list_rows(pool.clone(), params(Some(now - Duration::hours(24)))).await;
        assert!(
            rows.is_empty(),
            "a re-projected three-week-old run must not flood the 24h window",
        );

        // It still shows up once the window actually covers its run time.
        let rows = list_rows(pool, params(Some(now - Duration::days(30)))).await;
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

        for pattern in ["pc-9", "^pc-9$"] {
            let mut p = params(None);
            p.pc_id = Some(pattern.into());
            let rows = list_rows(pool.clone(), p).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].result_id, "r-out");
            assert_eq!(
                rows[0].stdout, "hello stdout",
                "winners must be re-hydrated with their output ({pattern})",
            );
            assert_eq!(rows[0].stderr, "hello stderr");
        }
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

        let fast = list_rows(pool.clone(), params(None)).await;
        let mut p = params(None);
        p.pc_id = Some("pc-7".into());
        let via_regex = list_rows(pool, p).await;

        assert_eq!(fast.len(), 1);
        assert_eq!(via_regex.len(), 1);
        assert_eq!(
            serde_json::to_value(&fast[0]).unwrap(),
            serde_json::to_value(&via_regex[0]).unwrap(),
            "metadata projection + hydration must reproduce the full row exactly",
        );
    }

    /// #590 regression: an in-flight row (events.started landed, no
    /// ExecResult yet → NULL exit_code/finished_at, and an ad-hoc run
    /// with NULL exec_id/job_id/version) must come back with `None` for
    /// every nullable column. The bug was `try_get(..).ok()`: sqlx-sqlite
    /// decodes a NULL `i64` to `0` and a NULL `String` to `""` instead of
    /// erroring, so `.ok()` produced `Some(0)` / `Some("")` — rendering a
    /// still-running command as "exit 0 / success" in the Activity list.
    #[tokio::test]
    async fn in_flight_row_keeps_nullable_columns_none() {
        let pool = fresh_pool().await;
        let now = Utc::now();
        // started_at set, everything else (exit_code, finished_at, exec_id,
        // job_id, version) left NULL — the shape the events.started insert
        // creates before the matching ExecResult lands.
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, stdout, stderr,
                 started_at, recorded_at)
             VALUES ('r-running', 'req', 'pc-1', '', '', ?, ?)",
        )
        .bind(now - Duration::minutes(1))
        .bind(now - Duration::minutes(1))
        .execute(&pool)
        .await
        .unwrap();

        let rows = list_rows(pool, params(None)).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.exit_code, None,
            "NULL exit_code must stay None, not decode to Some(0) / success",
        );
        assert_eq!(row.finished_at, None, "NULL finished_at must stay None");
        assert_eq!(
            row.exec_id, None,
            "NULL exec_id (ad-hoc run) must stay None, not Some(\"\")",
        );
        assert_eq!(
            row.job_id, None,
            "NULL job_id must stay None, not Some(\"\")"
        );
        assert_eq!(
            row.version, None,
            "NULL version must stay None, not Some(\"\")",
        );
    }

    /// `failure` must not swallow the agent's skips (the script never
    /// ran), which get their own `skipped` bucket — keyed on the flag, so
    /// a script that really exited 127, and the agent's unflagged
    /// signature refusal (123), are still failures. The three
    /// finished buckets partition the finished rows, and the row carries
    /// the flag for the SPA badge.
    #[tokio::test]
    async fn status_filter_splits_skips_from_failures() {
        let pool = fresh_pool().await;
        let now = Utc::now();
        for (id, exit, skipped) in [
            ("ok", 0, false),
            ("fail", 1, false),
            ("not-found", 127, false),
            ("refused", 123, false),
            ("unsupported", 122, true),
            ("deadline", 125, true),
        ] {
            sqlx::query(
                "INSERT INTO execution_results
                    (result_id, request_id, pc_id, exit_code, skipped, stdout, stderr,
                     started_at, finished_at, recorded_at)
                 VALUES (?, 'req', 'pc-1', ?, ?, '', '', ?, ?, ?)",
            )
            .bind(id)
            .bind(exit)
            .bind(skipped)
            .bind(now - Duration::minutes(10))
            .bind(now - Duration::minutes(9))
            .bind(now - Duration::minutes(9))
            .execute(&pool)
            .await
            .unwrap();
        }
        let rows = |status: StatusFilter| {
            let pool = pool.clone();
            async move {
                let mut p = params(None);
                p.status = Some(status);
                let mut rows: Vec<(String, bool)> = list_rows(pool, p)
                    .await
                    .into_iter()
                    .map(|r| (r.result_id, r.skipped))
                    .collect();
                rows.sort();
                rows
            }
        };
        let row = |id: &str, skipped: bool| (id.to_string(), skipped);
        assert_eq!(rows(StatusFilter::Success).await, [row("ok", false)]);
        assert_eq!(
            rows(StatusFilter::Failure).await,
            [
                row("fail", false),
                row("not-found", false),
                row("refused", false)
            ]
        );
        assert_eq!(
            rows(StatusFilter::Skipped).await,
            [row("deadline", true), row("unsupported", true)]
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
        let rows = list_rows(pool, p).await;
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

        let rows = list_rows(pool, params(Some(Utc::now() - Duration::hours(24)))).await;
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

        // Exercise both the unanchored metadata-regex rehydration path
        // ("pc-9") and the exact-literal SQL pushdown path ("^pc-9$").
        for pattern in ["pc-9", "^pc-9$"] {
            let mut p = params(None);
            p.pc_id = Some(pattern.into());
            let rows = list_rows(pool.clone(), p).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].stdout.chars().count(),
                PREVIEW_CHARS,
                "rehydrated output must still be clipped to the preview ({pattern})",
            );
            assert!(
                rows[0].stdout_truncated,
                "a clipped rehydrated stdout must be flagged ({pattern})",
            );
        }
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
        let rows = list_rows(pool, p).await;
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
        let rows = list_rows(pool, p).await;
        let m = rows[0].stdout_match.as_ref().unwrap();
        assert_eq!(
            m.matched.chars().count(),
            SNIPPET_MATCH_CHARS,
            "matched run is capped",
        );
        assert!(m.clipped_end, "a capped match flags clipped_end");
    }

    fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Seed `count` newer unrelated rows (`pc-noise`, `job-noise`,
    /// `exec-noise`, `stdout = 'ok'`) with strictly descending
    /// `started_at` timestamps above `older_than`.
    async fn seed_newer_unrelated_rows(
        pool: &SqlitePool,
        count: usize,
        older_than: chrono::DateTime<chrono::Utc>,
    ) {
        sqlx::query(
            "WITH RECURSIVE cnt(i) AS (
                 SELECT 1
                 UNION ALL
                 SELECT i + 1 FROM cnt WHERE i < ?
             )
             INSERT INTO execution_results
                 (result_id, request_id, exec_id, pc_id, job_id, exit_code,
                  stdout, stderr, started_at, finished_at, recorded_at)
             SELECT
                 printf('noise-%06d', i),
                 'req-noise',
                 'exec-noise',
                 'pc-noise',
                 'job-noise',
                 0,
                 'all quiet',
                 '',
                 strftime('%Y-%m-%dT%H:%M:%f', ?, printf('+%d seconds', i)) || '+00:00',
                 strftime('%Y-%m-%dT%H:%M:%f', ?, printf('+%d seconds', i + 1)) || '+00:00',
                 strftime('%Y-%m-%dT%H:%M:%f', ?, printf('+%d seconds', i + 1)) || '+00:00'
             FROM cnt",
        )
        .bind(count as i64)
        .bind(older_than)
        .bind(older_than)
        .bind(older_than)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn extract_exact_literal_is_conservative() {
        // Anchored plain and escaped-punctuation literals push down.
        assert_eq!(
            extract_exact_literal(Some("^PC001$")).as_deref(),
            Some("PC001")
        );
        assert_eq!(
            extract_exact_literal(Some("  ^job\\.\\+\\-\\[1\\]$  ")).as_deref(),
            Some("job.+-[1]")
        );
        // Unanchored or half-anchored literals must NOT become SQL `=`.
        assert_eq!(extract_exact_literal(Some("PC001")), None);
        assert_eq!(extract_exact_literal(Some("^PC001")), None);
        assert_eq!(extract_exact_literal(Some("PC001$")), None);
        // Empty anchored pattern matches NULL on job_id/exec_id in Rust
        // and must NOT become `col = ''`.
        assert_eq!(extract_exact_literal(Some("^$")), None);
        // Flags, alternations, groups, quantifiers, character classes, and
        // escaped trailing `$` stay on the Rust regex path.
        assert_eq!(extract_exact_literal(Some("^(?i)PC001$")), None);
        assert_eq!(extract_exact_literal(Some("(?i)^PC001$")), None);
        assert_eq!(extract_exact_literal(Some("^PC001|PC002$")), None);
        assert_eq!(extract_exact_literal(Some("^(PC001)$")), None);
        assert_eq!(extract_exact_literal(Some("^PC.*$")), None);
        assert_eq!(extract_exact_literal(Some("^PC\\d+$")), None);
        assert_eq!(extract_exact_literal(Some("^PC001\\$")), None);
    }

    /// #1666 regression: when more than `MAX_FETCH` (10,000) newer
    /// unrelated rows exist, an exact anchored identifier filter
    /// (`^...$`) pushes down to SQL equality (finding the old match on page
    /// 1), while a general metadata regex reports `scan_limit_reached` +
    /// `X-Results-Next-Cursor` and reaches the older match on page 2.
    #[tokio::test]
    async fn older_metadata_match_beyond_10k_newer_rows_is_reachable() {
        let pool = fresh_pool().await;
        let base = Utc::now() - Duration::days(10);

        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, exec_id, pc_id, job_id, exit_code,
                 stdout, stderr, started_at, finished_at, recorded_at)
             VALUES ('old-target', 'req-old', 'exec-old-42', 'pc-old-42', 'job.old+42', 0,
                     'old stdout payload', '', ?, ?, ?)",
        )
        .bind(base)
        .bind(base + Duration::seconds(1))
        .bind(base + Duration::seconds(1))
        .execute(&pool)
        .await
        .unwrap();

        seed_newer_unrelated_rows(&pool, MAX_FETCH + 5, base).await;

        // 1. Exact identifier regexes push down to SQL and find the old row
        //    immediately without exhausting the 10k scan window.
        let mut exact_p = params(Some(base - Duration::hours(1)));
        exact_p.pc_id = Some("^pc-old-42$".into());
        exact_p.job_id = Some("^job\\.old\\+42$".into());
        exact_p.exec_id = Some("^exec-old-42$".into());
        let (headers, Json(rows)) = list(State(pool.clone()), Query(exact_p)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_id, "old-target");
        assert_eq!(rows[0].stdout, "old stdout payload");
        assert_eq!(header_str(&headers, HEADER_SCANNED), Some("1"));
        assert_eq!(
            header_str(&headers, HEADER_STOP_REASON),
            Some("end_of_range")
        );
        assert!(headers.get(HEADER_NEXT_CURSOR).is_none());

        // 2. General (unanchored substring) regex scans 10,000 rows on page 1,
        //    signals `scan_limit_reached` with a cursor, and finds the old row
        //    on page 2.
        let mut regex_p = params(Some(base - Duration::hours(1)));
        regex_p.job_id = Some("old\\+42".into());
        let (headers_p1, Json(rows_p1)) = list(State(pool.clone()), Query(regex_p)).await.unwrap();
        assert!(
            rows_p1.is_empty(),
            "first 10k window contains only newer unrelated rows"
        );
        assert_eq!(header_str(&headers_p1, HEADER_SCANNED), Some("10000"));
        assert_eq!(
            header_str(&headers_p1, HEADER_STOP_REASON),
            Some("scan_limit_reached")
        );
        let cursor = header_str(&headers_p1, HEADER_NEXT_CURSOR)
            .expect("scan_limit_reached must provide a continuation cursor")
            .to_string();

        let mut regex_p2 = params(Some(base - Duration::hours(1)));
        regex_p2.job_id = Some("old\\+42".into());
        regex_p2.cursor = Some(cursor);
        let (headers_p2, Json(rows_p2)) = list(State(pool), Query(regex_p2)).await.unwrap();
        assert_eq!(rows_p2.len(), 1);
        assert_eq!(rows_p2[0].result_id, "old-target");
        assert_eq!(rows_p2[0].stdout, "old stdout payload");
        assert_eq!(header_str(&headers_p2, HEADER_SCANNED), Some("6"));
        assert_eq!(
            header_str(&headers_p2, HEADER_STOP_REASON),
            Some("end_of_range")
        );
        assert!(headers_p2.get(HEADER_NEXT_CURSOR).is_none());
    }

    /// #1666 regression: when more than `MAX_FETCH_WITH_OUTPUT` (1,000)
    /// newer unrelated rows exist, a `stdout` / `stderr` regex search
    /// scans 1,000 rows in page 1 (`scan_limit_reached`) and reaches the
    /// older match (with match snippet) on page 2 via `cursor`.
    #[tokio::test]
    async fn older_output_match_beyond_1k_newer_rows_is_reachable() {
        let pool = fresh_pool().await;
        let base = Utc::now() - Duration::days(5);
        let needle_out = format!("{}ANCIENT_NEEDLE{}", "p".repeat(PREVIEW_CHARS + 30), "q");

        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, job_id, exit_code,
                 stdout, stderr, started_at, finished_at, recorded_at)
             VALUES ('old-output-hit', 'req-old', 'pc-1', 'job-1', 1,
                     ?, 'FATAL_OLD_STDERR', ?, ?, ?)",
        )
        .bind(&needle_out)
        .bind(base)
        .bind(base + Duration::seconds(1))
        .bind(base + Duration::seconds(1))
        .execute(&pool)
        .await
        .unwrap();

        seed_newer_unrelated_rows(&pool, MAX_FETCH_WITH_OUTPUT + 4, base).await;

        let mut p1 = params(Some(base - Duration::hours(1)));
        p1.stdout = Some("ANCIENT_NEEDLE".into());
        p1.stderr = Some("FATAL_OLD".into());
        let (h1, Json(rows1)) = list(State(pool.clone()), Query(p1)).await.unwrap();
        assert!(rows1.is_empty());
        assert_eq!(header_str(&h1, HEADER_SCANNED), Some("1000"));
        assert_eq!(
            header_str(&h1, HEADER_STOP_REASON),
            Some("scan_limit_reached")
        );
        let cursor = header_str(&h1, HEADER_NEXT_CURSOR).unwrap().to_string();

        let mut p2 = params(Some(base - Duration::hours(1)));
        p2.stdout = Some("ANCIENT_NEEDLE".into());
        p2.stderr = Some("FATAL_OLD".into());
        p2.cursor = Some(cursor);
        let (h2, Json(rows2)) = list(State(pool), Query(p2)).await.unwrap();
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0].result_id, "old-output-hit");
        assert_eq!(
            rows2[0].stdout_match.as_ref().unwrap().matched,
            "ANCIENT_NEEDLE"
        );
        assert_eq!(rows2[0].stderr_match.as_ref().unwrap().matched, "FATAL_OLD");
        assert_eq!(header_str(&h2, HEADER_SCANNED), Some("5"));
        assert_eq!(header_str(&h2, HEADER_STOP_REASON), Some("end_of_range"));
        assert!(h2.get(HEADER_NEXT_CURSOR).is_none());
    }

    /// #1666: preserve regex filter semantics — unanchored literals match
    /// substrings, inline flags like `(?i)` match case-insensitively, and
    /// NULL `job_id` / `exec_id` values match `^$` as empty strings while
    /// being excluded by `^foo$`.
    #[tokio::test]
    async fn regex_semantics_for_substrings_flags_and_nulls_are_preserved() {
        let pool = fresh_pool().await;
        let now = Utc::now();
        for (id, pc, job, exec) in [
            ("r-sub", "PC0010", Some("deploy-prod"), Some("abc-123")),
            ("r-exact", "PC001", None, None),
        ] {
            sqlx::query(
                "INSERT INTO execution_results
                    (result_id, request_id, exec_id, pc_id, job_id, exit_code,
                     stdout, stderr, started_at, finished_at, recorded_at)
                 VALUES (?, 'req', ?, ?, ?, 0, '', '', ?, ?, ?)",
            )
            .bind(id)
            .bind(exec)
            .bind(pc)
            .bind(job)
            .bind(now - Duration::minutes(5))
            .bind(now - Duration::minutes(4))
            .bind(now - Duration::minutes(4))
            .execute(&pool)
            .await
            .unwrap();
        }

        // Unanchored `PC001` must match both `PC001` and `PC0010`.
        let mut p = params(None);
        p.pc_id = Some("PC001".into());
        let rows = list_rows(pool.clone(), p).await;
        assert_eq!(rows.len(), 2);

        // Anchored `^PC001$` matches only `PC001`.
        let mut p = params(None);
        p.pc_id = Some("^PC001$".into());
        let rows = list_rows(pool.clone(), p).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_id, "r-exact");

        // Case-insensitive flag `(?i)^pc001$` stays on Rust regex path and matches.
        let mut p = params(None);
        p.pc_id = Some("(?i)^pc001$".into());
        let rows = list_rows(pool.clone(), p).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_id, "r-exact");

        // `^$` on nullable `job_id` / `exec_id` matches the NULL row (`r-exact`).
        let mut p = params(None);
        p.job_id = Some("^$".into());
        p.exec_id = Some("^$".into());
        let rows = list_rows(pool, p).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_id, "r-exact");
    }

    /// #1666: equal `started_at` timestamps (e.g. a broadcast fan-out)
    /// paginate deterministically by `(started_at DESC, result_id DESC)`
    /// without duplicating or dropping rows across cursor boundaries.
    #[tokio::test]
    async fn equal_timestamp_rows_paginate_without_skips_or_duplicates() {
        let pool = fresh_pool().await;
        let same_instant = Utc::now() - Duration::minutes(30);
        for id in ["r-a", "r-b", "r-c", "r-d", "r-e"] {
            sqlx::query(
                "INSERT INTO execution_results
                    (result_id, request_id, pc_id, job_id, exit_code,
                     stdout, stderr, started_at, finished_at, recorded_at)
                 VALUES (?, 'req', 'pc-fanout', 'job-fanout', 0, '', '', ?, ?, ?)",
            )
            .bind(id)
            .bind(same_instant)
            .bind(same_instant)
            .bind(same_instant)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut p = params(None);
            p.limit = 2;
            p.pc_id = Some("fanout".into()); // unanchored regex path
            p.cursor = cursor.clone();
            let (headers, Json(rows)) = list(State(pool.clone()), Query(p)).await.unwrap();
            collected.extend(rows.into_iter().map(|r| r.result_id));
            match header_str(&headers, HEADER_NEXT_CURSOR) {
                Some(next) => {
                    assert_eq!(
                        header_str(&headers, HEADER_STOP_REASON),
                        Some("limit_reached")
                    );
                    cursor = Some(next.to_string());
                }
                None => {
                    assert_eq!(
                        header_str(&headers, HEADER_STOP_REASON),
                        Some("end_of_range")
                    );
                    break;
                }
            }
        }

        assert_eq!(collected, vec!["r-e", "r-d", "r-c", "r-b", "r-a"]);
    }

    /// #1666: when `limit` is reached mid-chunk during a Rust-regex scan,
    /// the cursor must point to the last *inspected* row rather than the
    /// end of the fetched SQL chunk so uninspected rows in that chunk are
    /// not skipped on the next page.
    #[tokio::test]
    async fn mid_chunk_stop_does_not_skip_uninspected_fetched_rows() {
        let pool = fresh_pool().await;
        let base = Utc::now() - Duration::hours(2);

        // Insert 10 rows (all fetched in the first 1,000-row chunk) where
        // every even row matches `pc-even`. With `limit = 2`, page 1 stops
        // after inspecting 4 rows; page 2 must still see the remaining
        // matching rows from that same chunk.
        for i in 1..=10 {
            let pc = if i % 2 == 0 { "pc-even" } else { "pc-odd" };
            let ts = base + Duration::seconds(i);
            sqlx::query(
                "INSERT INTO execution_results
                    (result_id, request_id, pc_id, exit_code, stdout, stderr,
                     started_at, finished_at, recorded_at)
                 VALUES (?, 'req', ?, 0, '', '', ?, ?, ?)",
            )
            .bind(format!("row-{i:02}"))
            .bind(pc)
            .bind(ts)
            .bind(ts)
            .bind(ts)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut p1 = params(None);
        p1.limit = 2;
        p1.pc_id = Some("even".into());
        let (h1, Json(r1)) = list(State(pool.clone()), Query(p1)).await.unwrap();
        assert_eq!(
            r1.iter().map(|r| r.result_id.as_str()).collect::<Vec<_>>(),
            vec!["row-10", "row-08"]
        );
        // Descending order: row-10 (match), row-09 (miss), row-08 (match) -> 3 inspected.
        assert_eq!(header_str(&h1, HEADER_SCANNED), Some("3"));
        assert_eq!(header_str(&h1, HEADER_STOP_REASON), Some("limit_reached"));
        let c1 = header_str(&h1, HEADER_NEXT_CURSOR).unwrap().to_string();

        let mut p2 = params(None);
        p2.limit = 2;
        p2.pc_id = Some("even".into());
        p2.cursor = Some(c1);
        let (h2, Json(r2)) = list(State(pool.clone()), Query(p2)).await.unwrap();
        assert_eq!(
            r2.iter().map(|r| r.result_id.as_str()).collect::<Vec<_>>(),
            vec!["row-06", "row-04"]
        );
        assert_eq!(header_str(&h2, HEADER_SCANNED), Some("4"));
        assert_eq!(header_str(&h2, HEADER_STOP_REASON), Some("limit_reached"));
        let c2 = header_str(&h2, HEADER_NEXT_CURSOR).unwrap().to_string();

        let mut p3 = params(None);
        p3.limit = 2;
        p3.pc_id = Some("even".into());
        p3.cursor = Some(c2);
        let (h3, Json(r3)) = list(State(pool), Query(p3)).await.unwrap();
        assert_eq!(
            r3.iter().map(|r| r.result_id.as_str()).collect::<Vec<_>>(),
            vec!["row-02"]
        );
        assert_eq!(header_str(&h3, HEADER_SCANNED), Some("3"));
        assert_eq!(header_str(&h3, HEADER_STOP_REASON), Some("end_of_range"));
        assert!(h3.get(HEADER_NEXT_CURSOR).is_none());
    }

    /// #1666: when the total number of rows in the range equals
    /// `MAX_FETCH_WITH_OUTPUT` (1,000) and no older row exists behind
    /// them, lookahead must report `end_of_range` (no cursor) rather than
    /// a false `scan_limit_reached`.
    #[tokio::test]
    async fn exact_budget_at_end_of_range_reports_end_of_range() {
        let pool = fresh_pool().await;
        let base = Utc::now() - Duration::hours(5);
        seed_newer_unrelated_rows(&pool, MAX_FETCH_WITH_OUTPUT, base).await;

        let mut p = params(None);
        p.stdout = Some("NO_SUCH_OUTPUT".into());
        let (headers, Json(rows)) = list(State(pool), Query(p)).await.unwrap();
        assert!(rows.is_empty());
        assert_eq!(header_str(&headers, HEADER_SCANNED), Some("1000"));
        assert_eq!(
            header_str(&headers, HEADER_STOP_REASON),
            Some("end_of_range")
        );
        assert!(
            headers.get(HEADER_NEXT_CURSOR).is_none(),
            "exact budget with no 1001st row must not emit a continuation cursor"
        );
    }

    /// #1666: `until` acts as an inclusive upper bound on `started_at`,
    /// and invalid `limit`, out-of-range years, inverted `since > until`,
    /// or malformed / mismatched cursors return `400 Bad Request`.
    #[tokio::test]
    async fn validates_bounds_limits_and_cursors() {
        let pool = fresh_pool().await;
        let t1 = Utc::now() - Duration::minutes(20);
        let t2 = Utc::now() - Duration::minutes(10);
        for (id, ts) in [("r-older", t1), ("r-newer", t2)] {
            sqlx::query(
                "INSERT INTO execution_results
                    (result_id, request_id, pc_id, exit_code, stdout, stderr,
                     started_at, finished_at, recorded_at)
                 VALUES (?, 'req', 'pc-1', 0, '', '', ?, ?, ?)",
            )
            .bind(id)
            .bind(ts)
            .bind(ts)
            .bind(ts)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Inclusive `until = t1` keeps `r-older` and excludes `r-newer`.
        let mut p = params(Some(t1));
        p.until = Some(t1);
        let rows = list_rows(pool.clone(), p).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_id, "r-older");

        // Invalid limits (0 and > MAX_LIMIT).
        for bad_limit in [0, MAX_LIMIT + 1] {
            let mut p = params(None);
            p.limit = bad_limit;
            let err = list(State(pool.clone()), Query(p)).await.err().unwrap();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
        }

        // Inverted `since > until`.
        let mut p = params(Some(t2));
        p.until = Some(t1);
        let err = list(State(pool.clone()), Query(p)).await.err().unwrap();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // Out-of-range 5-digit year on `since` or `until`.
        use chrono::TimeZone;
        let year_10000 = Utc.with_ymd_and_hms(10_000, 1, 1, 0, 0, 0).unwrap();
        let err = list(State(pool.clone()), Query(params(Some(year_10000))))
            .await
            .err()
            .unwrap();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let mut p = params(None);
        p.until = Some(year_10000);
        let err = list(State(pool.clone()), Query(p)).await.err().unwrap();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // Malformed / empty / overlong cursors.
        for bad_cursor in ["", "not-valid-base64!!!", &"a".repeat(MAX_CURSOR_LEN + 1)] {
            let mut p = params(None);
            p.cursor = Some(bad_cursor.to_string());
            let err = list(State(pool.clone()), Query(p)).await.err().unwrap();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
        }

        // Cursor minted for one filter set must be rejected when reused
        // with a different filter set.
        let mut p1 = params(None);
        p1.limit = 1;
        p1.pc_id = Some("pc-1".into());
        let (h1, _) = list(State(pool.clone()), Query(p1)).await.unwrap();
        let cursor = header_str(&h1, HEADER_NEXT_CURSOR).unwrap().to_string();

        let mut p_mismatch = params(None);
        p_mismatch.limit = 1;
        p_mismatch.pc_id = Some("pc-2".into());
        p_mismatch.cursor = Some(cursor);
        let err = list(State(pool), Query(p_mismatch)).await.err().unwrap();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn legacy_sqlite_timestamps_continue_without_reformatting() {
        let pool = fresh_pool().await;
        for id in ["a", "b", "c"] {
            insert_row(&pool, id).await;
        }
        for timestamp in ["2026-01-02 03:04:05", "2026-01-02 03:04:05.123456"] {
            sqlx::query("UPDATE execution_results SET started_at = ?")
                .bind(timestamp)
                .execute(&pool)
                .await
                .unwrap();
            let mut p = params(None);
            p.limit = 1;
            let (headers, rows) = list(State(pool.clone()), Query(p)).await.unwrap();
            assert_eq!(rows.0[0].result_id, "c");
            let cursor = header_str(&headers, HEADER_NEXT_CURSOR).unwrap();
            let hash = compute_filter_hash(&params(None));
            assert_eq!(
                decode_cursor(cursor, &hash, None, None).unwrap(),
                (timestamp.into(), "c".into())
            );
            let mut p = params(None);
            p.limit = 1;
            p.cursor = Some(cursor.into());
            let rows = list_rows(pool.clone(), p).await;
            assert_eq!(rows[0].result_id, "b");
            let lower = "2026-01-03T00:00:00Z".parse().unwrap();
            let upper = "2026-01-01T00:00:00Z".parse().unwrap();
            assert!(decode_cursor(cursor, &hash, Some(lower), None).is_err());
            assert!(decode_cursor(cursor, &hash, None, Some(upper)).is_err());
        }
        let hash = compute_filter_hash(&params(None));
        for bad in ["invalid", "2026-02-30 03:04:05", "10000-01-01 00:00:00"] {
            let cursor = encode_cursor(bad, "c", &hash).unwrap();
            assert!(decode_cursor(&cursor, &hash, None, None).is_err());
        }
    }

    #[tokio::test]
    async fn search_http_keeps_array_body_and_rejects_malformed_bounds() {
        use axum::{Router, body::Body, routing::get};
        use tower::ServiceExt;
        let pool = fresh_pool().await;
        insert_row(&pool, "a").await;
        let app = Router::new()
            .route("/api/results", get(list))
            .with_state(pool);
        for query in [
            "since=invalid",
            "until=invalid",
            "limit=0",
            "limit=1001",
            "limit=-1",
            "cursor=bad",
            "cursor=",
            "since=2026-02-01T00:00:00Z&until=2026-01-01T00:00:00Z",
            "until=%2B10000-01-01T00:00:00Z",
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/api/results?{query}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        }
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/results")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[HEADER_STOP_REASON], "end_of_range");
        assert_eq!(response.headers()[HEADER_SCANNED], "1");
        assert!(!response.headers().contains_key(HEADER_NEXT_CURSOR));
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(rows.is_array());
        assert_eq!(rows[0]["result_id"], "a");
    }

    /// #1666: composite identifier indexes serve exact identifier filters
    /// + `ORDER BY started_at DESC, result_id DESC` directly without
    /// falling back to `USE TEMP B-TREE FOR ORDER BY`.
    #[tokio::test]
    async fn exact_identifier_queries_use_composite_started_at_indexes() {
        let pool = fresh_pool().await;
        for (column, expected_idx) in [
            ("pc_id", "idx_execution_results_pc_started_at"),
            ("job_id", "idx_execution_results_job_started_at"),
            ("exec_id", "idx_execution_results_exec_started_at"),
        ] {
            let filters = SqlFilters {
                status: None,
                since: None,
                until: None,
                pc_exact: (column == "pc_id").then_some("pc-1"),
                job_exact: (column == "job_id").then_some("job-1"),
                exec_exact: (column == "exec_id").then_some("exec-1"),
            };
            let after = ("2026-10-30T00:00:00Z".into(), "r1".into());
            for select in [
                "SELECT * FROM execution_results",
                &format!("SELECT {META_COLUMNS} FROM execution_results"),
            ] {
                let mut qb = search_query(select, &filters, Some(&after), 50);
                let sql = format!("EXPLAIN QUERY PLAN {}", qb.sql().as_str());
                use sqlx::Execute;
                let plan_rows = sqlx::query_with(
                    sqlx::AssertSqlSafe(sql),
                    qb.build().take_arguments().unwrap().unwrap(),
                )
                .fetch_all(&pool)
                .await
                .unwrap();
                let details: Vec<String> = plan_rows
                    .into_iter()
                    .map(|r| r.try_get::<String, _>("detail").unwrap_or_default())
                    .collect();
                let joined = details.join(" | ");
                assert!(
                    joined.contains(expected_idx),
                    "expected query plan to use {expected_idx}, got: {joined}",
                );
                assert!(
                    !joined.contains("USE TEMP B-TREE"),
                    "expected index-ordered scan without temp B-tree sort, got: {joined}",
                );
            }
        }
    }
}
