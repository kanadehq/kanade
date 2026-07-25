//! Fleet-wide perf aggregates (v0.41 / Phase 3).
//!
//! Three endpoints, all driven off the existing `host_perf_samples`
//! and `process_perf_samples` time-series:
//!
//! * `GET /api/perf/fleet` — bucketed AVG / MAX of one metric across
//!   every PC in the fleet. Drives the Dashboard sparkline card.
//! * `GET /api/perf/top` — top-N PCs ranked by one metric averaged
//!   over a recent window. Drives the Dashboard "Top-5" cards.
//! * `GET /api/perf/active-investigations` — PCs currently
//!   publishing process_perf samples (i.e. an operator has them in
//!   investigation mode right now). Drives the "currently
//!   investigating" card so a forgotten ON toggle is visible at a
//!   glance.

use std::str::FromStr;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::warn;

use super::time_bounds::bounds_in_range;

/// One-hour default window mirrors `/api/agents/{pc_id}/perf`. The
/// Dashboard's "24h sparkline" sends `from=` explicitly so it
/// doesn't ride this default — the default is there for curl
/// inspection convenience.
const DEFAULT_WINDOW_SECS: i64 = 60 * 60;
/// 5 min bucket — same default as the per-PC endpoint.
const DEFAULT_STEP_SECS: i64 = 5 * 60;
/// Same hard ceiling as `/api/agents/{pc_id}/perf` to keep a
/// runaway `from=1y&step=1s` query from generating millions of
/// rows. Fleet aggregation is one row per bucket regardless of PC
/// count, so the same ceiling is conservatively safe.
const MAX_BUCKETS: i64 = 10_000;

/// Top-N defaults. 5 min is wide enough that a host that just sent
/// one stale sample doesn't dominate, narrow enough that the
/// ranking reflects "what's happening right now". 5 entries is the
/// Dashboard card's display budget.
const DEFAULT_TOP_WINDOW_SECS: i64 = 5 * 60;
const DEFAULT_TOP_LIMIT: i64 = 5;
const MAX_TOP_LIMIT: i64 = 50;

/// 5 min look-back for "active investigations" — the agent publishes
/// process_perf at the host_perf_interval cadence (default 60 s), so
/// 5 min covers ~5 consecutive ticks. A PC that genuinely stopped
/// publishing 5 min ago is no longer investigating from the
/// operator's standpoint.
const ACTIVE_INVESTIGATION_WINDOW_SECS: i64 = 5 * 60;

/// Whitelist of metric column names the API will splice into the
/// bucket query. Spliced as a literal (not bound) because SQLite
/// can't parameterise column identifiers; the enum-style match
/// keeps that splice injection-safe.
#[derive(Clone, Copy, Debug)]
enum Metric {
    CpuPct,
    MemUsedBytes,
    DiskReadBytesPerSec,
    DiskWrittenBytesPerSec,
    NetRxBytesPerSec,
    NetTxBytesPerSec,
}

impl Metric {
    fn column(self) -> &'static str {
        match self {
            Self::CpuPct => "cpu_pct",
            Self::MemUsedBytes => "mem_used_bytes",
            Self::DiskReadBytesPerSec => "disk_read_bytes_per_sec",
            Self::DiskWrittenBytesPerSec => "disk_written_bytes_per_sec",
            Self::NetRxBytesPerSec => "net_rx_bytes_per_sec",
            Self::NetTxBytesPerSec => "net_tx_bytes_per_sec",
        }
    }
}

impl FromStr for Metric {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cpu_pct" | "cpu" => Ok(Self::CpuPct),
            "mem_used_bytes" | "mem" | "memory" => Ok(Self::MemUsedBytes),
            "disk_read_bytes_per_sec" | "disk_read" => Ok(Self::DiskReadBytesPerSec),
            "disk_written_bytes_per_sec" | "disk_written" => Ok(Self::DiskWrittenBytesPerSec),
            "net_rx_bytes_per_sec" | "net_rx" => Ok(Self::NetRxBytesPerSec),
            "net_tx_bytes_per_sec" | "net_tx" => Ok(Self::NetTxBytesPerSec),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Aggregate {
    Avg,
    Max,
}

impl Aggregate {
    fn sql(self) -> &'static str {
        match self {
            Self::Avg => "AVG",
            Self::Max => "MAX",
        }
    }
}

impl FromStr for Aggregate {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "avg" | "mean" => Ok(Self::Avg),
            "max" => Ok(Self::Max),
            _ => Err(()),
        }
    }
}

// ----- /api/perf/fleet -----

#[derive(Deserialize)]
pub struct FleetPerfQuery {
    metric: Option<String>,
    agg: Option<String>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    step: Option<String>,
}

#[derive(Serialize)]
pub struct FleetPerfPoint {
    pub at: DateTime<Utc>,
    pub value: Option<f64>,
}

#[derive(Serialize)]
pub struct FleetPerfResponse {
    pub metric: String,
    pub agg: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub step_seconds: i64,
    pub points: Vec<FleetPerfPoint>,
}

pub async fn fleet(
    State(pool): State<SqlitePool>,
    Query(q): Query<FleetPerfQuery>,
) -> Result<Json<FleetPerfResponse>, StatusCode> {
    let metric = Metric::from_str(q.metric.as_deref().unwrap_or("cpu_pct"))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let agg = Aggregate::from_str(q.agg.as_deref().unwrap_or("avg"))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    // Issue #1126: `from`/`to` gate the string-stored `at` column
    // byte-wise; an expanded-year bound would invert the window. Reject
    // before defaults are applied. (The MAX_BUCKETS ceiling below only
    // catches wide windows, not a narrow one anchored at a huge year.)
    if !bounds_in_range([q.from, q.to]) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let to = q.to.unwrap_or_else(Utc::now);
    let from = q
        .from
        .unwrap_or_else(|| to - Duration::seconds(DEFAULT_WINDOW_SECS));
    let step_secs = match q.step.as_deref() {
        None => DEFAULT_STEP_SECS,
        Some(raw) => match humantime::parse_duration(raw) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(DEFAULT_STEP_SECS),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        },
    };
    if step_secs <= 0 || from >= to {
        return Err(StatusCode::BAD_REQUEST);
    }
    if (to - from).num_seconds() / step_secs > MAX_BUCKETS {
        return Err(StatusCode::BAD_REQUEST);
    }

    // `metric.column()` and `agg.sql()` are spliced as literals
    // because SQLite can't parameterise column / function names.
    // Both come from a closed enum match above, so the splice is
    // injection-safe.
    let sql = format!(
        "SELECT
             (CAST(strftime('%s', at) AS INTEGER) / ?) * ? AS bucket_unix,
             {agg}({metric}) AS value
         FROM host_perf_samples
         WHERE at >= ? AND at < ?
         GROUP BY bucket_unix
         ORDER BY bucket_unix ASC",
        agg = agg.sql(),
        metric = metric.column(),
    );

    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(step_secs)
        .bind(step_secs)
        .bind(from)
        .bind(to)
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, metric = ?metric, "fleet perf query");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let points = rows
        .into_iter()
        .map(|r| {
            let bucket: i64 = r.try_get("bucket_unix").unwrap_or(0);
            FleetPerfPoint {
                at: DateTime::<Utc>::from_timestamp(bucket, 0).unwrap_or(from),
                value: r.try_get("value").ok(),
            }
        })
        .collect();

    Ok(Json(FleetPerfResponse {
        metric: metric.column().to_string(),
        agg: match agg {
            Aggregate::Avg => "avg".into(),
            Aggregate::Max => "max".into(),
        },
        from,
        to,
        step_seconds: step_secs,
        points,
    }))
}

// ----- /api/perf/top -----

#[derive(Deserialize)]
pub struct TopPerfQuery {
    metric: Option<String>,
    window: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct TopPerfRow {
    pub pc_id: String,
    pub hostname: Option<String>,
    pub value: f64,
}

#[derive(Serialize)]
pub struct TopPerfResponse {
    pub metric: String,
    pub window_seconds: i64,
    pub rows: Vec<TopPerfRow>,
}

pub async fn top(
    State(pool): State<SqlitePool>,
    Query(q): Query<TopPerfQuery>,
) -> Result<Json<TopPerfResponse>, StatusCode> {
    let metric = Metric::from_str(q.metric.as_deref().unwrap_or("cpu_pct"))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let window_secs = match q.window.as_deref() {
        None => DEFAULT_TOP_WINDOW_SECS,
        Some(raw) => match humantime::parse_duration(raw) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(DEFAULT_TOP_WINDOW_SECS),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        },
    };
    if window_secs <= 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let limit = q.limit.unwrap_or(DEFAULT_TOP_LIMIT).clamp(1, MAX_TOP_LIMIT);

    // LEFT JOIN agents so hostnames render alongside pc_id where
    // available — falls back to NULL for hosts that haven't sent a
    // heartbeat yet. Window cutoff via `at > datetime('now', '-Ns')`
    // is a literal because SQLite's relative-time format wants the
    // sign inside the modifier.
    let from = Utc::now() - Duration::seconds(window_secs);
    let sql = format!(
        "SELECT h.pc_id,
                a.hostname AS hostname,
                AVG(h.{metric}) AS value
         FROM host_perf_samples h
         LEFT JOIN agents a ON a.pc_id = h.pc_id
         WHERE h.at > ?
           AND h.{metric} IS NOT NULL
         GROUP BY h.pc_id
         ORDER BY value DESC NULLS LAST
         LIMIT ?",
        metric = metric.column(),
    );

    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(from)
        .bind(limit)
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, metric = ?metric, "top perf query");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let rows = rows
        .into_iter()
        .map(|r| TopPerfRow {
            pc_id: r.try_get("pc_id").unwrap_or_default(),
            hostname: r.try_get("hostname").ok(),
            value: r.try_get("value").unwrap_or(0.0),
        })
        .collect();

    Ok(Json(TopPerfResponse {
        metric: metric.column().to_string(),
        window_seconds: window_secs,
        rows,
    }))
}

// ----- /api/perf/active-investigations -----

#[derive(Serialize)]
pub struct ActiveInvestigation {
    pub pc_id: String,
    pub hostname: Option<String>,
    pub latest_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct ActiveInvestigationsResponse {
    pub window_seconds: i64,
    pub rows: Vec<ActiveInvestigation>,
}

pub async fn active_investigations(
    State(pool): State<SqlitePool>,
) -> Result<Json<ActiveInvestigationsResponse>, StatusCode> {
    let from = Utc::now() - Duration::seconds(ACTIVE_INVESTIGATION_WINDOW_SECS);
    // A PC is "actively investigating" if it has published any
    // process_perf sample in the last window. Aggregating MAX(at)
    // also surfaces the freshest sample so the SPA can show "last
    // sample N s ago" alongside the badge.
    let rows = sqlx::query(
        "SELECT p.pc_id, a.hostname AS hostname, MAX(p.at) AS latest_at
         FROM process_perf_samples p
         LEFT JOIN agents a ON a.pc_id = p.pc_id
         WHERE p.at > ?
         GROUP BY p.pc_id
         ORDER BY latest_at DESC",
    )
    .bind(from)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "active_investigations query");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let rows = rows
        .into_iter()
        .filter_map(|r| {
            let pc_id: String = r.try_get("pc_id").ok()?;
            let latest_at: DateTime<Utc> = r.try_get("latest_at").ok()?;
            Some(ActiveInvestigation {
                pc_id,
                hostname: r.try_get("hostname").ok(),
                latest_at,
            })
        })
        .collect();

    Ok(Json(ActiveInvestigationsResponse {
        window_seconds: ACTIVE_INVESTIGATION_WINDOW_SECS,
        rows,
    }))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn bounds_query(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> FleetPerfQuery {
        FleetPerfQuery {
            metric: None,
            agg: None,
            from,
            to,
            step: None,
        }
    }

    // A bare `host_perf_samples` so the in-range control query runs for
    // real against a live DB — the string-compared `at` gate and the
    // strftime bucketing only execute there.
    async fn seeded_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE host_perf_samples ( \
               id INTEGER PRIMARY KEY AUTOINCREMENT, pc_id TEXT NOT NULL, \
               at TIMESTAMP NOT NULL, cpu_pct REAL )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO host_perf_samples (pc_id, at, cpu_pct) VALUES (?, ?, ?)")
            .bind("p1")
            .bind(Utc.with_ymd_and_hms(2026, 6, 17, 10, 0, 0).unwrap())
            .bind(50.0_f64)
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    // Issue #1126: an expanded-year *upper* bound is the silently-wrong
    // case here — a too-large `from` is already caught by the `from >= to`
    // ordering check, but a year-10000 `to` sorts below every stored row
    // and used to answer an empty window as `200 OK`. It must now 400. (An
    // empty pool suffices: the guard fires before the query, so neutralising
    // it would fall through to a table-missing 500 — a clean mutation signal.)
    #[tokio::test]
    async fn fleet_rejects_expanded_year_to_bound() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let q = bounds_query(
            None,
            Some(Utc.with_ymd_and_hms(10000, 1, 1, 0, 0, 0).unwrap()),
        );
        let res = fleet(State(pool), Query(q)).await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
    }

    // Control: an ordinary window straddling the seeded row is accepted and
    // returns it — proves the guard doesn't reject legitimate bounds.
    #[tokio::test]
    async fn fleet_accepts_in_range_bounds() {
        let pool = seeded_pool().await;
        // A narrow window straddling the 10:00 row (wide windows trip the
        // MAX_BUCKETS ceiling, an unrelated 400).
        let q = bounds_query(
            Some(Utc.with_ymd_and_hms(2026, 6, 17, 9, 0, 0).unwrap()),
            Some(Utc.with_ymd_and_hms(2026, 6, 17, 11, 0, 0).unwrap()),
        );
        let res = fleet(State(pool), Query(q))
            .await
            .expect("in-range bounds must be accepted");
        assert_eq!(res.0.points.len(), 1);
    }
}
