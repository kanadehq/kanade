//! Compliance read API (#290 PR-E). `GET /api/checks` returns the
//! fleet-wide `check_status` rows the SPA Compliance page renders —
//! which PCs pass / warn / fail / unknown for each operator-defined
//! `check:` job (those with `fleet` enabled; `fleet: false` checks stay
//! client-only and never project here). One row per (pc_id, check),
//! latest status — not a time series.
//!
//! #497: the page's whole point is spotting failing PCs, but the
//! response used to carry every (pc, check) row — at fleet scale
//! that's 3,000 × K rows per 60 s poll when the healthy bulk is
//! `ok`. The default response now carries only the attention rows
//! (`status != 'ok'`) plus complete per-check status COUNTS (so the
//! badges stay fleet-true); the ok bulk is fetched on demand per
//! check via `?check=<name>&include_ok=true`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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

/// Per-check fleet rollup — complete regardless of row filtering, so
/// the SPA badges always show true fleet numbers.
#[derive(Serialize, Default, Clone)]
pub struct CheckCounts {
    pub check_name: String,
    pub ok: i64,
    pub warn: i64,
    pub fail: i64,
    pub unknown: i64,
}

#[derive(Serialize)]
pub struct ChecksResponse {
    pub counts: Vec<CheckCounts>,
    pub rows: Vec<CheckRow>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChecksParams {
    /// Narrow rows + counts to one check — the SPA's per-card
    /// expand-on-demand path.
    pub check: Option<String>,
    /// Include `ok` rows. Default false: the attention rows are the
    /// page's purpose, and the ok bulk dominates a healthy fleet.
    pub include_ok: Option<bool>,
}

/// Row query shared verbatim with the unit tests, so the tests can't
/// silently diverge from what the handler executes (PR #565 review,
/// claude).
const ROWS_SQL: &str = "SELECT pc_id, check_name, status, detail, recorded_at
         FROM check_status
         WHERE (?1 IS NULL OR check_name = ?1)
           AND (?2 OR status != 'ok')
         ORDER BY check_name, pc_id";

/// `GET /api/checks` — attention rows + complete per-check counts.
/// The SPA groups rows into the fleet matrix (check × PC).
pub async fn list_all(
    State(state): State<AppState>,
    Query(params): Query<ChecksParams>,
) -> Result<Json<ChecksResponse>, (StatusCode, String)> {
    let include_ok = params.include_ok.unwrap_or(false);
    let check = params
        .check
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Counts first: complete per (check, status) regardless of the
    // row filter, so the badges can't drift from reality. Typed
    // query_as so a column rename / type mismatch is a loud 500,
    // not silently-zeroed badges (PR #565 review, claude).
    #[derive(sqlx::FromRow)]
    struct CountRow {
        check_name: String,
        status: String,
        n: i64,
    }
    let count_rows: Vec<CountRow> = sqlx::query_as(
        "SELECT check_name, status, COUNT(*) AS n
         FROM check_status
         WHERE (?1 IS NULL OR check_name = ?1)
         GROUP BY check_name, status",
    )
    .bind(check)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "check_status count query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // HashMap accumulator — grouping must not be load-bearing on the
    // SQL result order (PR #565 review, claude).
    let mut by_check: HashMap<String, CheckCounts> = HashMap::new();
    for r in count_rows {
        let entry = by_check
            .entry(r.check_name.clone())
            .or_insert_with(|| CheckCounts {
                check_name: r.check_name,
                ..CheckCounts::default()
            });
        match r.status.as_str() {
            "ok" => entry.ok = r.n,
            "warn" => entry.warn = r.n,
            "fail" => entry.fail = r.n,
            // The projector normalises to four states; anything else
            // (a future state from a newer projector) rolls into
            // `unknown` rather than vanishing.
            _ => entry.unknown += r.n,
        }
    }
    let mut counts: Vec<CheckCounts> = by_check.into_values().collect();
    counts.sort_by(|a, b| a.check_name.cmp(&b.check_name));

    // `query_as` propagates real sqlx decode errors (type mismatch,
    // missing column) instead of the `try_get(...).ok()` idiom that
    // silently defaults them away.
    let rows: Vec<CheckRow> = sqlx::query_as(ROWS_SQL)
        .bind(check)
        .bind(include_ok)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "check_status query");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    Ok(Json(ChecksResponse { counts, rows }))
}

// AppState carries NATS handles that can't be constructed in a unit
// test, so these tests exercise the exact SQL list_all binds.
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn seeded_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for (pc, check, status) in [
            ("pc-1", "bitlocker", "ok"),
            ("pc-2", "bitlocker", "fail"),
            ("pc-3", "bitlocker", "ok"),
            ("pc-1", "av", "warn"),
            ("pc-2", "av", "ok"),
        ] {
            sqlx::query(
                "INSERT INTO check_status (pc_id, check_name, status, recorded_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(pc)
            .bind(check)
            .bind(status)
            .bind(chrono::Utc::now())
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    async fn rows_for(pool: &SqlitePool, check: Option<&str>, include_ok: bool) -> Vec<CheckRow> {
        sqlx::query_as(ROWS_SQL)
            .bind(check)
            .bind(include_ok)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn default_rows_exclude_ok() {
        let pool = seeded_pool().await;
        let rows = rows_for(&pool, None, false).await;
        assert_eq!(rows.len(), 2, "only warn+fail rows by default");
        assert!(rows.iter().all(|r| r.status != "ok"));
    }

    #[tokio::test]
    async fn check_filter_with_include_ok_returns_full_check() {
        let pool = seeded_pool().await;
        let rows = rows_for(&pool, Some("bitlocker"), true).await;
        assert_eq!(rows.len(), 3, "all bitlocker rows incl. ok");
        assert!(rows.iter().all(|r| r.check_name == "bitlocker"));
    }
}
