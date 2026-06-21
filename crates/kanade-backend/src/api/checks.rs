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
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::AppState;
use crate::audit;
use crate::audit::Caller;

#[derive(Serialize, sqlx::FromRow)]
pub struct CheckRow {
    pub pc_id: String,
    pub check_name: String,
    /// Operator-authored display title (`CheckHint.label`); `None` ⇒ the
    /// SPA falls back to the `check_name` slug.
    pub label: Option<String>,
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
    /// Display title for the check's card; mirrors [`CheckRow::label`]
    /// so an all-ok check (no attention rows) still gets a titled card.
    pub label: Option<String>,
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
const ROWS_SQL: &str = "SELECT pc_id, check_name, label, status, detail, recorded_at
         FROM check_status
         WHERE (?1 IS NULL OR check_name = ?1)
           AND (?2 OR status != 'ok')
         ORDER BY check_name, pc_id";

/// Delete query shared with the unit tests (same rationale as
/// [`ROWS_SQL`]). `?2 IS NULL` ⇒ clear the check across every PC;
/// bound non-null ⇒ clear just that one PC's row.
const CLEAR_SQL: &str = "DELETE FROM check_status
         WHERE check_name = ?1
           AND (?2 IS NULL OR pc_id = ?2)";

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
        // `MAX(label)`: label is per check_name (every row of a check
        // shares the hint's title), so the max over a (check, status)
        // group is just that title — and it's non-null whenever any
        // contributing row carried one.
        label: Option<String>,
        status: String,
        n: i64,
    }
    let count_rows: Vec<CountRow> = sqlx::query_as(
        "SELECT check_name, MAX(label) AS label, status, COUNT(*) AS n
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
        // Any group's non-null label is the check's title; keep the
        // first one seen so a status group with no rows can't blank it.
        if entry.label.is_none() {
            entry.label = r.label;
        }
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

#[derive(Debug, Default, Deserialize)]
pub struct ClearParams {
    /// Clear only this PC's row for the check. Omit to clear the check
    /// across every PC — the common case for a deleted / renamed check
    /// whose status rows are now orphaned on the Compliance page.
    pub pc_id: Option<String>,
}

#[derive(Serialize)]
pub struct ClearResponse {
    pub deleted: u64,
}

/// `DELETE /api/checks/{check_name}` — drop stored `check_status` rows
/// for a check. Deleting the *job* that produced a check never touched
/// these rows: jobs live in NATS KV, status in SQLite keyed by
/// `(pc_id, check_name)` with no job link, so a removed / renamed check
/// leaves orphaned rows on the Compliance page indefinitely. This is the
/// operator's explicit "clear it". By design it is NOT auto-cascaded
/// from job delete — a same-named replacement job legitimately keeps
/// writing the slug, and observed state shouldn't vanish as a side
/// effect of a config edit (and a slug *rename* never hits the delete
/// path at all). `?pc_id=` scopes the clear to one PC.
pub async fn clear(
    State(state): State<AppState>,
    Path(check_name): Path<String>,
    Query(params): Query<ClearParams>,
    caller: Caller,
) -> Result<Json<ClearResponse>, (StatusCode, String)> {
    let check_name = check_name.trim();
    if check_name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "check_name must be non-empty".into(),
        ));
    }
    let pc_id = params
        .pc_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let deleted = sqlx::query(CLEAR_SQL)
        .bind(check_name)
        .bind(pc_id)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            warn!(error = %e, check_name, "check_status delete");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?
        .rows_affected();

    audit::record(
        &state.nats,
        "operator",
        "check_clear",
        Some(check_name),
        Some(&caller),
        serde_json::json!({ "check_name": check_name, "pc_id": pc_id, "deleted": deleted }),
    )
    .await;

    Ok(Json(ClearResponse { deleted }))
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
        // `bitlocker` carries a label; `av` doesn't — exercises both the
        // titled and slug-fallback paths through CheckRow / CheckCounts.
        for (pc, check, label, status) in [
            ("pc-1", "bitlocker", Some("BitLocker 暗号化"), "ok"),
            ("pc-2", "bitlocker", Some("BitLocker 暗号化"), "fail"),
            ("pc-3", "bitlocker", Some("BitLocker 暗号化"), "ok"),
            ("pc-1", "av", None, "warn"),
            ("pc-2", "av", None, "ok"),
        ] {
            sqlx::query(
                "INSERT INTO check_status (pc_id, check_name, label, status, recorded_at)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(pc)
            .bind(check)
            .bind(label)
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
    async fn rows_carry_label_and_fall_back_to_none() {
        let pool = seeded_pool().await;
        let rows = rows_for(&pool, None, true).await;
        let bl = rows.iter().find(|r| r.check_name == "bitlocker").unwrap();
        assert_eq!(bl.label.as_deref(), Some("BitLocker 暗号化"));
        let av = rows.iter().find(|r| r.check_name == "av").unwrap();
        assert_eq!(av.label, None, "unlabeled check leaves label NULL");
    }

    #[tokio::test]
    async fn check_filter_with_include_ok_returns_full_check() {
        let pool = seeded_pool().await;
        let rows = rows_for(&pool, Some("bitlocker"), true).await;
        assert_eq!(rows.len(), 3, "all bitlocker rows incl. ok");
        assert!(rows.iter().all(|r| r.check_name == "bitlocker"));
    }

    #[tokio::test]
    async fn clear_removes_every_pc_for_a_check() {
        let pool = seeded_pool().await;
        let deleted = sqlx::query(CLEAR_SQL)
            .bind("bitlocker")
            .bind(Option::<&str>::None)
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(deleted, 3, "all three bitlocker rows cleared");
        assert!(rows_for(&pool, Some("bitlocker"), true).await.is_empty());
        // A different check is left untouched.
        assert_eq!(rows_for(&pool, Some("av"), true).await.len(), 2);
    }

    #[tokio::test]
    async fn clear_scoped_to_one_pc() {
        let pool = seeded_pool().await;
        let deleted = sqlx::query(CLEAR_SQL)
            .bind("bitlocker")
            .bind(Some("pc-2"))
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(deleted, 1, "only pc-2's bitlocker row cleared");
        let left = rows_for(&pool, Some("bitlocker"), true).await;
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|r| r.pc_id != "pc-2"));
    }
}
