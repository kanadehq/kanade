//! Issue #246 — `/api/obs_events*` routes.
//!
//! Drives the SPA Events page (per-PC timeline) and the planned
//! fleet-wide observability dashboard. Three endpoints:
//!
//! - `GET /api/obs_events?pc_id=&from=&to=&kind=&source=&limit=`
//!   Filtered list, ordered by `at DESC`. Filters are optional;
//!   omitting them all returns the most recent events fleet-wide.
//! - `GET /api/obs_events/kinds` — distinct `kind` strings the
//!   SPA's filter chip needs to populate without a separate query.
//! - `GET /api/obs_events/recent?limit=` — convenience alias for
//!   "newest N events fleet-wide" (same as `obs_events` with no
//!   `pc_id` and `limit` default 50).
//!
//! Pagination is keyset (`before_id`) rather than offset so a long-
//! tail timeline view doesn't drift when new events arrive between
//! pages — matches the inventory and audit endpoints' shape.
//! Pagination is deferred to the SPA PR; the first cut returns
//! up to `limit` rows in a single call.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use tracing::warn;

/// Default page size when the caller doesn't specify `limit`.
/// Generous enough to render a "today on this PC" table without
/// pagination chrome; cheap enough server-side that an accidental
/// no-filter call doesn't pull millions of rows.
const DEFAULT_LIMIT: i64 = 200;
/// Hard ceiling. A misbehaving caller asking for `limit=1_000_000`
/// would otherwise exhaust SQLite's working memory on a busy fleet.
const MAX_LIMIT: i64 = 5_000;

#[derive(Deserialize)]
pub struct ListQuery {
    pub pc_id: Option<String>,
    /// RFC3339 lower bound (inclusive).
    pub from: Option<DateTime<Utc>>,
    /// RFC3339 upper bound (exclusive).
    pub to: Option<DateTime<Utc>>,
    /// Exact-match filter on `kind` (e.g. `logon`, `boot`).
    pub kind: Option<String>,
    /// Exact-match filter on `source` (e.g. `winlog:Security`).
    pub source: Option<String>,
    /// Exact-match filter on `payload.logon_type` (Issue #366).
    /// Windows LogonType numbers: 2 interactive, 3 network,
    /// 4 batch, 5 service, 7 unlock, 10 RDP, 11 cached. Only
    /// logon/logoff events carry the field, so combining this
    /// with a non-logon `kind` filter returns nothing — which is
    /// the honest answer.
    pub logon_type: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct EventRow {
    pub id: i64,
    pub pc_id: String,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub source: String,
    pub event_record_id: Option<String>,
    /// Parsed back into a JSON Value so the SPA receives structured
    /// data instead of a stringified JSON column.
    pub payload: serde_json::Value,
}

#[derive(Serialize)]
pub struct ListResponse {
    pub events: Vec<EventRow>,
}

/// `GET /api/obs_events`.
pub async fn list(
    State(pool): State<SqlitePool>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, StatusCode> {
    let limit = match q.limit {
        None => DEFAULT_LIMIT,
        Some(n) if n > 0 && n <= MAX_LIMIT => n,
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    // Static SQL with "param IS NULL OR column = param" gates per
    // optional filter. Equivalent to building a dynamic WHERE on
    // the fly but keeps the SQL string a `&'static str`, which is
    // what `kanade-backend`'s lint config requires (dynamic SQL is
    // blocked at the lint level to prevent accidental injection
    // surfaces). The same binding appears twice per gated filter
    // — once for the NULL check, once for the equality — which is
    // a SQLite query-planner no-op (the `IS NULL` branch
    // short-circuits at parse time when the bind is non-NULL).
    // `json_extract` on the `?6` gate: `payload` is stored as JSON
    // text, so the logon_type filter digs into it at query time.
    // No index on the expression — acceptable because the filter
    // composes with the indexed gates above and the table is
    // cleanup-bounded (see cleanup.rs).
    let rows = sqlx::query(
        "SELECT id, pc_id, at, kind, source, event_record_id, payload
         FROM obs_events
         WHERE (?1 IS NULL OR pc_id  = ?1)
           AND (?2 IS NULL OR at    >= ?2)
           AND (?3 IS NULL OR at    <  ?3)
           AND (?4 IS NULL OR kind   = ?4)
           AND (?5 IS NULL OR source = ?5)
           AND (?6 IS NULL OR json_extract(payload, '$.logon_type') = ?6)
         ORDER BY at DESC, id DESC
         LIMIT ?7",
    )
    .bind(q.pc_id.as_deref())
    .bind(q.from)
    .bind(q.to)
    .bind(q.kind.as_deref())
    .bind(q.source.as_deref())
    .bind(q.logon_type)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "obs_events list query");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let events = rows
        .into_iter()
        .filter_map(|r| match row_to_event(&r) {
            Ok(e) => Some(e),
            Err(e) => {
                // Gemini #248 HIGH: surface schema mismatches /
                // type errors instead of returning blank fields.
                // We can't propagate the error here without
                // changing the response shape; warn-log + drop
                // the row keeps the API consistent while making
                // any bug operator-visible via agent.log.
                warn!(error = %e, "obs_events: drop row that failed to decode");
                None
            }
        })
        .collect();

    Ok(Json(ListResponse { events }))
}

/// Decode one `obs_events` row into an `EventRow`. Errors propagate
/// (vs the previous `unwrap_or_default()` shape which silently
/// returned empty strings on a column-rename / type mismatch). The
/// caller drops the row + logs; an alternative would be to 500 the
/// whole response, but a single bad row in a 200-row page
/// shouldn't take the whole timeline down.
fn row_to_event(r: &SqliteRow) -> sqlx::Result<EventRow> {
    let raw: String = r.try_get("payload")?;
    // `payload` is JSON text we stored ourselves, so a parse
    // failure means data corruption (someone hand-edited the
    // table) rather than a schema mismatch — bubble the same
    // error type out so the warn-log captures both cases.
    let payload = serde_json::from_str(&raw).map_err(|e| {
        sqlx::Error::Decode(format!("obs_events.payload not valid JSON: {e}").into())
    })?;
    Ok(EventRow {
        id: r.try_get("id")?,
        pc_id: r.try_get("pc_id")?,
        at: r.try_get("at")?,
        kind: r.try_get("kind")?,
        source: r.try_get("source")?,
        event_record_id: r.try_get("event_record_id")?,
        payload,
    })
}

#[derive(Serialize)]
pub struct KindsResponse {
    pub kinds: Vec<String>,
}

/// `GET /api/obs_events/kinds`.
pub async fn kinds(State(pool): State<SqlitePool>) -> Result<Json<KindsResponse>, StatusCode> {
    let rows = sqlx::query("SELECT DISTINCT kind FROM obs_events ORDER BY kind")
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "obs_events kinds query");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    // Drop rows that fail to decode (same handling rationale as
    // `list` above — operator sees the warn, the API stays useful).
    let kinds = rows
        .into_iter()
        .filter_map(|r| match r.try_get::<String, _>("kind") {
            Ok(k) => Some(k),
            Err(e) => {
                warn!(error = %e, "obs_events kinds: drop row that failed to decode kind");
                None
            }
        })
        .collect();
    Ok(Json(KindsResponse { kinds }))
}

#[derive(Deserialize)]
pub struct RecentQuery {
    pub limit: Option<i64>,
}

/// `GET /api/obs_events/recent?limit=`. Convenience alias for
/// `/api/obs_events` with no `pc_id`. Lower default `limit` (50)
/// suited to a dashboard "latest activity" card.
pub async fn recent(
    State(pool): State<SqlitePool>,
    Query(q): Query<RecentQuery>,
) -> Result<Json<ListResponse>, StatusCode> {
    let limit = match q.limit {
        None => 50,
        Some(n) if n > 0 && n <= MAX_LIMIT => n,
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    // Same shape as `list` with no filters, just a different
    // default `limit`. Kept as a sibling handler (vs delegating to
    // `list`) so the response model + log labels stay specific to
    // "recent" — small clarity win over saving a few lines.
    let rows = sqlx::query(
        "SELECT id, pc_id, at, kind, source, event_record_id, payload
         FROM obs_events
         ORDER BY at DESC, id DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "obs_events recent query");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let events = rows
        .into_iter()
        .filter_map(|r| match row_to_event(&r) {
            Ok(e) => Some(e),
            Err(e) => {
                warn!(error = %e, "obs_events recent: drop row that failed to decode");
                None
            }
        })
        .collect();
    Ok(Json(ListResponse { events }))
}
