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
//! - `GET /api/obs_events/sources` — distinct `source` strings for
//!   the include/exclude chips (Issue #391).
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

use super::time_bounds::bounds_in_range;
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
    /// the honest answer. Kept for URL compatibility; the SPA now
    /// sends the generic `payload_key`/`payload_value` pair
    /// (Issue #391) instead.
    pub logon_type: Option<i64>,
    /// Issue #391: comma-separated include / exclude lists for
    /// `kind` and `source`. Both compose with the single-value
    /// `kind` / `source` gates above (which stay for URL
    /// compatibility); an empty include list (after splitting)
    /// means "no constraint", same as absent.
    pub kinds: Option<String>,
    pub kinds_ex: Option<String>,
    pub sources: Option<String>,
    pub sources_ex: Option<String>,
    /// Issue #391: generic payload filter — `payload_key=user` +
    /// `payload_value=yukimemi` matches rows whose
    /// `payload.<key> == <value>`. The key is restricted to
    /// `[A-Za-z0-9_]+` (400 otherwise) so the `'$.' || ?` path
    /// concat below can't be steered into other JSONPath syntax.
    /// The value is matched as text AND, when it parses as a
    /// number, numerically — so `payload_value=2` matches the
    /// JSON number 2 that the collectors emit.
    pub payload_key: Option<String>,
    pub payload_value: Option<String>,
    pub limit: Option<i64>,
}

/// Turn a comma-separated filter list into the JSON-array string
/// the `json_each(?)` binds expect. Blank segments are dropped;
/// an empty result collapses to `None` ("no constraint") so a
/// trailing comma can't accidentally filter everything out.
fn csv_to_json_array(csv: &Option<String>) -> Option<String> {
    let vals: Vec<&str> = csv
        .as_deref()?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if vals.is_empty() {
        return None;
    }
    // Values are data, not SQL — serde_json escaping keeps the
    // array well-formed whatever the kind/source strings contain.
    serde_json::to_string(&vals).ok()
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

    // Issue #1076/#1126: reject any lexically-uncomparable date bound
    // before it reaches the string-compared `at` gates below (see
    // `time_bounds`).
    if !bounds_in_range([q.from, q.to]) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Issue #391: payload key allow-list — the key is interpolated
    // into a JSONPath via `'$.' || ?`, so anything beyond
    // identifier characters (quotes, brackets, dots) is rejected
    // up front rather than left to SQLite's path parser.
    let payload_key = match q.payload_key.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(k) if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => Some(k),
        Some(_) => return Err(StatusCode::BAD_REQUEST),
    };
    // The pair only constrains when BOTH halves are present —
    // otherwise neutralise it entirely. A key without a value
    // would otherwise bind `?12` to NULL and the
    // `json_extract(...) = NULL` comparison blanks the whole
    // result set for direct API callers (Gemini #394 medium; the
    // SPA always sends both).
    let payload_value = payload_key.and(q.payload_value.as_deref());
    let payload_key = payload_value.and(payload_key);
    // Numeric twin: collectors emit numbers as JSON numbers
    // (logon_type: 2), and SQLite's `json_extract` returns them
    // typed — a text-only bind would never match. f64 covers i64
    // payload values under SQLite's numeric comparison rules.
    let payload_value_num: Option<f64> = payload_value.and_then(|v| v.parse().ok());

    let kinds_json = csv_to_json_array(&q.kinds);
    let kinds_ex_json = csv_to_json_array(&q.kinds_ex);
    let sources_json = csv_to_json_array(&q.sources);
    let sources_ex_json = csv_to_json_array(&q.sources_ex);

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
    // Issue #391 additions keep the same static-SQL discipline:
    // the include/exclude lists arrive as JSON-array strings and
    // unpack inside SQLite via `json_each(?)` — one bind per list,
    // no dynamic IN-clause assembly. The generic payload gate
    // builds its JSONPath from a validated identifier (`'$.' || ?`)
    // and compares against the text bind plus, when the value is
    // numeric, the f64 twin.
    let rows = sqlx::query(
        "SELECT id, pc_id, at, kind, source, event_record_id, payload
         FROM obs_events
         WHERE (?1 IS NULL OR pc_id  = ?1)
           AND (?2 IS NULL OR at    >= ?2)
           AND (?3 IS NULL OR at    <  ?3)
           AND (?4 IS NULL OR kind   = ?4)
           AND (?5 IS NULL OR source = ?5)
           AND (?6 IS NULL OR json_extract(payload, '$.logon_type') = ?6)
           AND (?7 IS NULL OR kind   IN     (SELECT value FROM json_each(?7)))
           AND (?8 IS NULL OR kind   NOT IN (SELECT value FROM json_each(?8)))
           AND (?9 IS NULL OR source IN     (SELECT value FROM json_each(?9)))
           AND (?10 IS NULL OR source NOT IN (SELECT value FROM json_each(?10)))
           AND (?11 IS NULL
                OR json_extract(payload, '$.' || ?11) = ?12
                OR (?13 IS NOT NULL AND json_extract(payload, '$.' || ?11) = ?13))
         ORDER BY at DESC, id DESC
         LIMIT ?14",
    )
    .bind(q.pc_id.as_deref())
    .bind(q.from)
    .bind(q.to)
    .bind(q.kind.as_deref())
    .bind(q.source.as_deref())
    .bind(q.logon_type)
    .bind(kinds_json)
    .bind(kinds_ex_json)
    .bind(sources_json)
    .bind(sources_ex_json)
    .bind(payload_key)
    .bind(payload_value)
    .bind(payload_value_num)
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

/// How many PCs one `lane_seeds` call may ask about. The swimlane draws at
/// most `CHART_MAX_PCS` (40) hosts, so this is that with headroom rather
/// than an arbitrary cap — a caller asking for more is asking for something
/// the strip cannot render.
const MAX_SEED_PCS: usize = 64;

#[derive(Deserialize)]
pub struct SeedsQuery {
    /// Comma-separated `pc_id`s — the hosts the strip is about to draw.
    pub pcs: String,
    /// The window start. Seeds are the newest event strictly before it.
    pub before: DateTime<Utc>,
}

/// `GET /api/obs_events/lane_seeds`.
///
/// The newest event before `before`, per PC and per swimlane lane.
///
/// The Events page fetches a WINDOW of events, so a host that did not reboot
/// inside it reports no power event at all, and the strip cannot tell "this
/// host has no winlog collector" from "this host simply stayed up" (#1256).
/// The Analytics `op_timeline` query has always seeded itself this way; this
/// gives the Events page the same footing so the two surfaces stop
/// disagreeing about the same host and window.
///
/// Deliberately a separate call rather than a wider `list`: seeding the whole
/// fleet would mean walking back per kind until every PC is covered, which a
/// host that last rebooted months ago makes unbounded. Scoped to the ≤40 PCs
/// actually drawn, each lookup is an index seek on `(pc_id, at DESC)` — the
/// same shape `op_timeline` already runs per PC.
pub async fn lane_seeds(
    State(pool): State<SqlitePool>,
    Query(q): Query<SeedsQuery>,
) -> Result<Json<ListResponse>, StatusCode> {
    let pcs: Vec<&str> = q
        .pcs
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pcs.is_empty() || pcs.len() > MAX_SEED_PCS {
        return Err(StatusCode::BAD_REQUEST);
    }

    // One seek per PC rather than a dynamic `IN (…)`: keeps the SQL static
    // (the same reason `op_timeline` spells its kind lists out) and bounded
    // by MAX_SEED_PCS.
    //
    // The lane CASE must stay aligned with `OP_LANES` in the SPA's
    // OperationalTimeline.tsx and with the `op_timeline` query in
    // analytics.rs — see `tests::lane_seed_kinds_match_op_timeline`.
    let mut events = Vec::new();
    for pc in pcs {
        let rows = sqlx::query(
            "SELECT id, pc_id, at, kind, source, event_record_id, payload FROM (                SELECT id, pc_id, at, kind, source, event_record_id, payload,                       ROW_NUMBER() OVER (                         PARTITION BY CASE                           WHEN kind IN ('boot', 'shutdown', 'unexpected_shutdown',                                         'log_service_started', 'log_service_stopped') THEN 'power'                           WHEN kind IN ('logon', 'logoff') THEN 'session'                           WHEN kind IN ('sleep', 'resume') THEN 'sleep'                           WHEN kind IN ('active', 'idle') THEN 'active'                         END                         ORDER BY at DESC                       ) AS rn                FROM obs_events                WHERE pc_id = ?1 AND at < ?2                  AND kind IN ('boot', 'shutdown', 'unexpected_shutdown',                               'log_service_started', 'log_service_stopped',                               'logon', 'logoff', 'sleep', 'resume',                               'active', 'idle')              ) WHERE rn = 1 ORDER BY at",
        )
        .bind(pc)
        .bind(q.before)
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, pc_id = %pc, "lane_seeds query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        for r in &rows {
            match row_to_event(r) {
                Ok(ev) => events.push(ev),
                // Same posture as `list`: one undecodable row must not cost
                // the whole strip its seeds.
                Err(e) => warn!(error = %e, "skipping undecodable lane seed"),
            }
        }
    }
    Ok(Json(ListResponse { events }))
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

#[derive(Serialize)]
pub struct SourcesResponse {
    pub sources: Vec<String>,
}

/// `GET /api/obs_events/sources` (Issue #391) — distinct `source`
/// strings for the SPA's include/exclude chips, mirroring `kinds`.
pub async fn sources(State(pool): State<SqlitePool>) -> Result<Json<SourcesResponse>, StatusCode> {
    let rows = sqlx::query("SELECT DISTINCT source FROM obs_events ORDER BY source")
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "obs_events sources query");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let sources = rows
        .into_iter()
        .filter_map(|r| match r.try_get::<String, _>("source") {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "obs_events sources: drop row that failed to decode source");
                None
            }
        })
        .collect();
    Ok(Json(SourcesResponse { sources }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use chrono::{TimeZone, Utc};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        // One ordinary row so a query that ISN'T rejected returns
        // something — that's what makes the "inverted filter dumps the
        // whole table" bug observable in the `Ok`-path assertions.
        sqlx::query(
            "INSERT INTO obs_events (pc_id, at, kind, source, event_record_id, payload)
             VALUES ('pc-01', ?, 'logon', 'winlog:Security', '1', '{}')",
        )
        .bind(Utc.with_ymd_and_hms(2026, 5, 28, 10, 41, 0).unwrap())
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// A `ListQuery` with only the two date bounds set — every other
    /// filter absent. Keeps the handler tests focused on #1076.
    fn bounds_query(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> ListQuery {
        ListQuery {
            pc_id: None,
            from,
            to,
            kind: None,
            source: None,
            logon_type: None,
            kinds: None,
            kinds_ex: None,
            sources: None,
            sources_ex: None,
            payload_key: None,
            payload_value: None,
            limit: None,
        }
    }

    // The `bound_in_range` unit test moved to `time_bounds`; the
    // handler-level guard tests below stay to prove `list` still 400s.

    #[tokio::test]
    async fn list_rejects_expanded_year_from_bound() {
        // The exact failure the issue reports: a year-10000 lower bound
        // used to sort below every row and return the whole table. It
        // must now 400 instead of silently dumping everything.
        let pool = fresh_pool().await;
        let q = bounds_query(
            Some(Utc.with_ymd_and_hms(10000, 1, 1, 0, 0, 0).unwrap()),
            None,
        );
        let res = list(State(pool), Query(q)).await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
    }

    #[tokio::test]
    async fn list_rejects_expanded_year_to_bound() {
        // The `to` side inverts the other way (answered 0 rows), equally
        // wrong — reject it too.
        let pool = fresh_pool().await;
        let q = bounds_query(
            None,
            Some(Utc.with_ymd_and_hms(10000, 1, 1, 0, 0, 0).unwrap()),
        );
        let res = list(State(pool), Query(q)).await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
    }

    #[tokio::test]
    async fn list_accepts_in_range_bounds() {
        // Control: an ordinary window straddling the seeded row is
        // accepted and returns it — proves the guard doesn't reject
        // legitimate bounds.
        let pool = fresh_pool().await;
        let q = bounds_query(
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            Some(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()),
        );
        let res = list(State(pool), Query(q))
            .await
            .expect("in-range bounds must be accepted");
        assert_eq!(res.0.events.len(), 1);
    }
    /// Insert one obs_event, with a unique `event_record_id` so the table's
    /// `UNIQUE(pc_id, source, event_record_id)` doesn't collapse the rows.
    async fn seed_row(pool: &SqlitePool, pc: &str, at: DateTime<Utc>, kind: &str, rec: &str) {
        sqlx::query(
            "INSERT INTO obs_events (pc_id, at, kind, source, event_record_id, payload)
             VALUES (?, ?, ?, 'winlog:System', ?, '{}')",
        )
        .bind(pc)
        .bind(at)
        .bind(kind)
        .bind(rec)
        .execute(pool)
        .await
        .unwrap();
    }

    /// The lane CASE in `lane_seeds` must group kinds exactly the way
    /// `op_timeline` (analytics.rs) and `OP_LANES` (OperationalTimeline.tsx)
    /// do. There are three copies of that mapping; this pins the one this
    /// module owns, the same way `analytics::tests::op_timeline_kind_set_is_stable`
    /// pins the query next door.
    ///
    /// Behavioural rather than string-matching on the SQL: it asserts what
    /// the grouping DOES — one seed per lane, the newest of each — so a CASE
    /// edited into a different shape fails even if it still parses.
    #[tokio::test]
    async fn lane_seed_kinds_match_op_timeline() {
        let pool = fresh_pool().await;
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 6, 17, h, 0, 0).unwrap();

        // Two events per lane before the window. The SECOND of each pair is
        // what a correct lane grouping returns.
        for (i, (older, newer)) in [
            ("boot", "shutdown"), // power
            ("logon", "logoff"),  // session
            ("sleep", "resume"),  // sleep
            ("active", "idle"),   // active
        ]
        .iter()
        .enumerate()
        {
            seed_row(&pool, "seedpc", at(i as u32 * 2), older, &format!("o{i}")).await;
            seed_row(
                &pool,
                "seedpc",
                at(i as u32 * 2 + 1),
                newer,
                &format!("n{i}"),
            )
            .await;
        }
        // Neither a lane kind nor an observation kind: must not seed anything.
        seed_row(&pool, "seedpc", at(15), "app_sample", "x1").await;
        // Observation kinds drive no lane, so they must not seed either — a
        // seeded `agent_online` would assert liveness the window never saw.
        seed_row(&pool, "seedpc", at(16), "agent_online", "x2").await;

        let res = lane_seeds(
            State(pool),
            Query(SeedsQuery {
                pcs: "seedpc".into(),
                before: Utc.with_ymd_and_hms(2026, 6, 18, 0, 0, 0).unwrap(),
            }),
        )
        .await
        .expect("lane_seeds must succeed");

        let mut got: Vec<&str> = res.0.events.iter().map(|e| e.kind.as_str()).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            ["idle", "logoff", "resume", "shutdown"],
            "one seed per lane, the newest of each — and nothing from a              non-lane kind"
        );
    }

    #[tokio::test]
    async fn lane_seeds_rejects_an_empty_or_oversized_pc_list() {
        let pool = fresh_pool().await;
        let before = Utc.with_ymd_and_hms(2026, 6, 18, 0, 0, 0).unwrap();
        for pcs in [
            "".to_string(),
            ",, ,".to_string(),
            (0..MAX_SEED_PCS + 1)
                .map(|i| format!("pc{i}"))
                .collect::<Vec<_>>()
                .join(","),
        ] {
            let res = lane_seeds(State(pool.clone()), Query(SeedsQuery { pcs, before })).await;
            assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
        }
    }
}
