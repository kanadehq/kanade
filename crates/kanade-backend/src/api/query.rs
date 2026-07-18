//! `POST /api/query` — operator ad-hoc **read-only** SQL over the
//! projector's SQLite database (admin-only).
//!
//! The projector DB already holds every cross-cut an operator might
//! want to slice — installed software (`inventory_sw_apps`), compliance
//! (`check_status`), run history (`execution_results`), observability
//! (`obs_events`), and every `explode:`-derived table — but until now
//! each question needed a bespoke endpoint. This is the generic escape
//! hatch: write a `SELECT`, get columns + rows back. It's also the
//! foundation the vulnerability dashboard rides on (matching installed
//! software against an imported feed is just a `JOIN`).
//!
//! ## Safety
//! Raw SQL is powerful, so the access is bounded on several axes:
//!   * **Read-only connection** — the handler runs on a dedicated pool
//!     opened with `SQLITE_OPEN_READONLY` (`AppState::query_pool`), so a
//!     stray `DELETE`/`DROP`/`UPDATE` fails at the SQLite layer, not just
//!     by convention.
//!   * **SELECT/WITH only** — [`validate_read_only`] rejects anything
//!     whose leading keyword isn't `SELECT`/`WITH`, blocks stacked
//!     statements (`;`), and blocks write verbs + `ATTACH`/`DETACH`/`PRAGMA`
//!     as whole words (catching a data-modifying CTE like
//!     `WITH x AS (…) DELETE …` that slips past the leading-keyword check,
//!     and `ATTACH`, which a read-only connection wouldn't fully contain).
//!     The check ignores string literals / quoted identifiers / comments,
//!     so it's belt-and-suspenders on top of the read-only pool, not a
//!     parser.
//!   * **Row cap** — results stream and stop at `limit` (+1 to flag
//!     truncation), so a `SELECT * FROM big_table` can't balloon memory.
//!   * **Wall-clock timeout** — the whole fetch is wrapped in a
//!     [`QUERY_TIMEOUT`] so a pathological scan can't pin a connection
//!     forever.
//!   * **Admin-only** — registered under the `require_admin` route layer
//!     (see `api::router`).

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, Column, Executor, Row, SqlSafeStr, SqlitePool, TypeInfo, ValueRef};
use tracing::warn;

use super::AppState;

/// Default cap on returned rows when the caller doesn't specify one.
const DEFAULT_LIMIT: usize = 1_000;
/// Hard ceiling on `limit` — a caller can't ask for an unbounded dump.
/// `pub(crate)` so callers that want the sandbox's own row cap (e.g. the
/// dynamic-group resolver in `api::group_sql`) reference it instead of
/// re-declaring the value and risking drift.
pub(crate) const MAX_LIMIT: usize = 10_000;
/// Wall-clock budget for a single query. A read-only connection plus the
/// row cap bound most damage; this catches a pathological full-table
/// scan that produces few rows but takes forever.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
pub struct QueryRequest {
    pub sql: String,
    /// Max rows to return (clamped to `1..=MAX_LIMIT`; default
    /// `DEFAULT_LIMIT`). The query streams and stops once the cap is hit.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct QueryResponse {
    /// Column names in result order. Read from the prepared statement
    /// (`describe`), so they're present even when the query returns zero
    /// rows.
    pub columns: Vec<String>,
    /// Row-major cells, each row aligned to `columns`. Cell JSON types
    /// follow the SQLite storage class of the value (integer / real /
    /// text / null); BLOBs are summarised as `"<blob N bytes>"` rather
    /// than dumped.
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    /// True when more rows existed than `limit` — the result was cut off.
    pub truncated: bool,
    pub elapsed_ms: u128,
}

/// Reject anything that isn't a single read-only `SELECT`/`WITH`. Returns
/// `Err(reason)` with an operator-facing message. This is defence in
/// depth on top of the read-only connection — see the module docs.
pub fn validate_read_only(sql: &str) -> Result<(), String> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err("empty query".into());
    }

    // Leading keyword must be SELECT or WITH (CTE). A query that opens
    // with a comment is rejected too — uncommon, and not worth the
    // comment-stripping surface to allow.
    let kw_end = trimmed
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let kw = trimmed[..kw_end].to_ascii_lowercase();
    if kw != "select" && kw != "with" {
        return Err(format!(
            "only read-only SELECT / WITH queries are allowed (got '{kw}')"
        ));
    }

    // The structural checks below run on a copy with string literals,
    // quoted identifiers, and comments stripped, so a `;` or a keyword
    // INSIDE a string (`WHERE notes LIKE '%;%'`, `WHERE action = 'delete'`)
    // or a quoted identifier (`"delete"`) can't trip a false positive
    // (claude / gemini #899).
    let stripped = strip_sql_noise(trimmed).to_ascii_lowercase();

    // Block stacked statements: a `;` anywhere but a single trailing one
    // is a second statement.
    if stripped.trim_end_matches(';').contains(';') {
        return Err("multiple statements are not allowed".into());
    }

    // Block write verbs + ATTACH/DETACH/PRAGMA/VACUUM as whole words. The
    // read-only connection is the real guarantee — a data-modifying CTE
    // (`WITH x AS (...) DELETE FROM t`) that slips past the SELECT/WITH
    // leading-keyword check still fails at the SQLite layer — but
    // rejecting these up front closes that gap with a clear message
    // instead of a raw SQLite error (gemini #899 security note). ATTACH
    // especially isn't fully contained by a read-only connection (it can
    // reach another DB file).
    for kw in [
        "insert", "update", "delete", "replace", "drop", "create", "alter", "attach", "detach",
        "pragma", "vacuum", "reindex",
    ] {
        if contains_word(&stripped, kw) {
            return Err(format!("'{kw}' is not allowed in a read-only query"));
        }
    }
    Ok(())
}

/// Replace string literals (`'…'`), quoted identifiers (`"…"`), and SQL
/// comments (`-- …`, `/* … */`) with spaces, leaving structural SQL
/// intact. The keyword + `;` scans in [`validate_read_only`] run on the
/// result so a literal `;` / `delete` inside a string or a `"delete"`
/// identifier doesn't read as the SQL token. Doubled quotes (`''`, `""`)
/// are SQL escapes and stay inside the literal.
fn strip_sql_noise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                let quote = c;
                while let Some(d) = chars.next() {
                    if d == quote {
                        // Doubled quote = escaped; consume it and stay inside.
                        if chars.peek() == Some(&quote) {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                out.push(' ');
            }
            '-' if chars.peek() == Some(&'-') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next(); // consume '*'
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// True if `needle` appears in `haystack` bounded by non-word characters
/// on both sides — so `attach` matches in `... attach ...` but not inside
/// `cattached`. `haystack` is assumed already lowercased. The boundary
/// chars are read as `char`s (not raw bytes), so a non-ASCII byte in a
/// UTF-8 identifier doesn't get mis-widened (claude #899).
fn contains_word(haystack: &str, needle: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let i = start + pos;
        let before_ok = haystack[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word(c));
        let after = i + needle.len();
        let after_ok = haystack[after..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return true;
        }
        start = after;
    }
    false
}

/// Decode one cell to JSON by switching on the value's SQLite storage
/// class. Reading `type_info()` off the raw value (rather than trying
/// `try_get::<i64>` → `<f64>` → `<String>` in turn) means one decode per
/// cell instead of up to three failed decodes, each of which allocates a
/// sqlx error (gemini #899). The NULL check is also why we read the raw
/// value first: sqlx-sqlite decodes a NULL as `0` for a non-`Option`
/// `i64`, so a typed decode would mislabel NULL as the integer 0.
fn decode_cell(row: &sqlx::sqlite::SqliteRow, i: usize) -> serde_json::Value {
    use serde_json::Value;
    let class = match row.try_get_raw(i) {
        Err(_) => return Value::Null,
        Ok(raw) if raw.is_null() => return Value::Null,
        Ok(raw) => raw.type_info().name().to_string(),
    };
    match class.as_str() {
        "INTEGER" => row
            .try_get::<i64, _>(i)
            .map(Value::from)
            .unwrap_or(Value::Null),
        "REAL" => row
            .try_get::<f64, _>(i)
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        "BLOB" => row
            .try_get::<Vec<u8>, _>(i)
            .map(|b| Value::String(format!("<blob {} bytes>", b.len())))
            .unwrap_or(Value::Null),
        // TEXT and anything unexpected: decode as text.
        _ => row
            .try_get::<String, _>(i)
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

/// A completed read-only query — columns (from `describe`, so present even at
/// zero rows), row-major cells, and whether the result was capped.
pub(crate) struct ReadOnlyResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool,
}

/// Why a [`run_read_only`] failed — kept structured so each caller maps it to
/// its own surface (HTTP status for `/api/query`, a per-widget error string
/// for the materialized views).
pub(crate) enum ReadOnlyError {
    /// The SQL failed [`validate_read_only`] (not a read-only SELECT/WITH).
    Rejected(String),
    /// The fetch exceeded [`QUERY_TIMEOUT`].
    Timeout,
    /// A SQLite error at run time (syntax, unknown table, or a write attempt
    /// caught by the read-only connection).
    Sql(String),
}

impl std::fmt::Display for ReadOnlyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadOnlyError::Rejected(m) => write!(f, "{m}"),
            ReadOnlyError::Timeout => {
                write!(f, "query exceeded the {}s budget", QUERY_TIMEOUT.as_secs())
            }
            ReadOnlyError::Sql(m) => write!(f, "query failed: {m}"),
        }
    }
}

/// Run one read-only query on the dedicated `SQLITE_OPEN_READONLY` pool,
/// validating it first and bounding it by row cap + wall-clock timeout. Shared
/// by the ad-hoc `POST /api/query` endpoint and the materialized `view:`
/// widgets, so both get the identical safety envelope.
///
/// `pc_id` binds the query's `:pc_id` token for a per-PC `view:` widget.
/// sqlx-sqlite only binds POSITIONAL `?` (not `:name`), so we rewrite every
/// `:pc_id` occurrence to `?` and bind the value once per occurrence — the
/// value only ever reaches SQLite through `.bind()`, never spliced into the
/// text, so a per-PC widget can't be an injection vector. `None` leaves the
/// query as-is (the fleet widgets and the ad-hoc `/api/query` endpoint).
pub(crate) async fn run_read_only(
    pool: &SqlitePool,
    sql: &str,
    limit: usize,
    pc_id: Option<&str>,
) -> Result<ReadOnlyResult, ReadOnlyError> {
    validate_read_only(sql).map_err(ReadOnlyError::Rejected)?;
    let limit = limit.clamp(1, MAX_LIMIT);
    // Rewrite the per-PC token to positional placeholders. `bind_count` is the
    // number of `?` we must bind `pc_id` to. The shared, literal-aware scanner
    // only rewrites real `:pc_id` tokens (not ones inside a string literal /
    // comment), so `bind_count` matches what SQLite sees. When `pc_id` is None
    // the SQL is untouched (bind_count 0).
    let (effective_sql, bind_count) = match pc_id {
        Some(_) => kanade_shared::manifest::rewrite_pc_id_param(sql),
        None => (sql.to_string(), 0),
    };
    let fetch = async {
        // Column names from `describe` (prepares without running), so a
        // zero-row result still reports its columns instead of an empty
        // header (claude #899).
        let described = pool
            .describe(AssertSqlSafe(effective_sql.clone()).into_sql_str())
            .await?;
        let columns: Vec<String> = described
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();

        // Stream + cap so a huge result set can't be materialised whole.
        // Pull one past `limit` to detect (and flag) truncation.
        let mut q = sqlx::query(AssertSqlSafe(effective_sql.clone()));
        if let Some(pc) = pc_id {
            for _ in 0..bind_count {
                q = q.bind(pc.to_string());
            }
        }
        let mut stream = q.fetch(pool);
        let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
        let mut truncated = false;
        while let Some(row) = stream.try_next().await? {
            if rows.len() >= limit {
                truncated = true;
                break;
            }
            let cells = (0..row.columns().len())
                .map(|i| decode_cell(&row, i))
                .collect();
            rows.push(cells);
        }
        Ok::<_, sqlx::Error>(ReadOnlyResult {
            columns,
            rows,
            truncated,
        })
    };

    match tokio::time::timeout(QUERY_TIMEOUT, fetch).await {
        Err(_) => Err(ReadOnlyError::Timeout),
        Ok(Err(e)) => {
            // A write attempt on the read-only connection lands here as a
            // SQLite error, as do syntax errors and unknown tables.
            warn!(error = %e, "read-only query failed");
            Err(ReadOnlyError::Sql(e.to_string()))
        }
        Ok(Ok(v)) => Ok(v),
    }
}

pub async fn execute(
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let started = Instant::now();
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT);
    let result = run_read_only(&state.query_pool, &req.sql, limit, None)
        .await
        .map_err(|e| {
            let status = match e {
                ReadOnlyError::Rejected(_) => StatusCode::BAD_REQUEST,
                ReadOnlyError::Timeout => StatusCode::REQUEST_TIMEOUT,
                ReadOnlyError::Sql(_) => StatusCode::BAD_REQUEST,
            };
            (status, e.to_string())
        })?;

    let row_count = result.rows.len();
    Ok(Json(QueryResponse {
        columns: result.columns,
        rows: result.rows,
        row_count,
        truncated: result.truncated,
        elapsed_ms: started.elapsed().as_millis(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn accepts_select_and_with() {
        validate_read_only("SELECT 1").unwrap();
        validate_read_only("  select * from t  ").unwrap();
        validate_read_only("WITH x AS (SELECT 1) SELECT * FROM x").unwrap();
        validate_read_only("SELECT 1;").unwrap(); // single trailing ; ok
        // Keywords / `;` INSIDE a string literal or quoted identifier are
        // data, not statements — must not trip the structural checks.
        validate_read_only("SELECT * FROM t WHERE notes LIKE '%;%'").unwrap();
        validate_read_only("SELECT * FROM audit WHERE action = 'delete'").unwrap();
        validate_read_only("SELECT \"delete\" FROM t").unwrap();
        validate_read_only("SELECT created_at, updated_at FROM t -- attach\n").unwrap();
    }

    #[test]
    fn rejects_writes_and_ddl() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "DROP TABLE t",
            "CREATE TABLE t (a)",
            "REPLACE INTO t VALUES (1)",
            "",
            "   ",
        ] {
            assert!(validate_read_only(sql).is_err(), "should reject: {sql:?}");
        }
    }

    #[test]
    fn rejects_stacked_statements() {
        assert!(validate_read_only("SELECT 1; DROP TABLE t").is_err());
        assert!(validate_read_only("SELECT 1; SELECT 2").is_err());
    }

    #[test]
    fn rejects_attach_detach_pragma_and_write_verbs() {
        assert!(validate_read_only("ATTACH DATABASE 'x' AS y").is_err());
        assert!(validate_read_only("SELECT * FROM t PRAGMA foo").is_err());
        assert!(validate_read_only("VACUUM").is_err());
        // A data-modifying CTE slips past the SELECT/WITH leading-keyword
        // check but is caught by the write-verb scan (gemini #899).
        assert!(validate_read_only("WITH x AS (SELECT 1) DELETE FROM t").is_err());
        // ...but a column/table that merely CONTAINS the substring is fine.
        validate_read_only("SELECT cattached FROM pragmatic_table").unwrap();
        validate_read_only("SELECT created_at, updated_at FROM deleted_log").unwrap();
    }

    #[test]
    fn contains_word_is_boundary_aware() {
        assert!(contains_word("select 1; attach 'x'", "attach"));
        assert!(!contains_word("select cattached from t", "attach"));
        assert!(!contains_word("select pragmatic from t", "pragma"));
        // A non-ASCII letter directly bordering the match is a word char,
        // so it must NOT count as a boundary — the old `byte as char` cast
        // mis-widened the trailing UTF-8 byte and would have matched here.
        assert!(!contains_word("cafédelete", "delete"));
    }

    #[test]
    fn strip_sql_noise_removes_strings_idents_comments() {
        assert_eq!(strip_sql_noise("a 'b;c' d"), "a   d");
        assert_eq!(strip_sql_noise("a \"x;y\" d"), "a   d");
        assert_eq!(strip_sql_noise("a -- b;c\nd"), "a \nd");
        assert_eq!(strip_sql_noise("a /* b;c */ d"), "a   d");
        // Doubled quote is an escape — stays one logical literal.
        assert_eq!(strip_sql_noise("'it''s' x"), "  x");
    }

    #[tokio::test]
    async fn decode_cell_covers_storage_classes() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let row = sqlx::query("SELECT 42 AS i, 1.5 AS r, 'hi' AS t, NULL AS n")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(decode_cell(&row, 0), serde_json::json!(42));
        assert_eq!(decode_cell(&row, 1), serde_json::json!(1.5));
        assert_eq!(decode_cell(&row, 2), serde_json::json!("hi"));
        assert_eq!(decode_cell(&row, 3), serde_json::Value::Null);
    }
}
