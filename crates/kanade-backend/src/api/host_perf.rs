//! `/api/agents/{pc_id}/perf` — per-PC host perf time-series.
//!
//! Bucketed in SQL so the response stays bounded regardless of how
//! wide the operator zooms. The default 5-minute bucket gives 288
//! points/24h or 8640 points/30d — comfortably under Recharts' "still
//! feels native" envelope (a few thousand) so the SPA renders without
//! down-sampling client-side.
//!
//! Aggregation is `AVG` for every metric. Ratios stay accurate
//! (`AVG(used)/AVG(total)` ≈ time-weighted occupancy because total is
//! near-constant within a host). Rates also average meaningfully —
//! 5-min mean B/s is what a network admin reads in MRTG / Cacti-style
//! graphs.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::warn;

/// Default time window when the caller doesn't specify `from`/`to`.
/// One hour matches the SPA's smallest range selector and keeps the
/// default response under ~720 rows even at 5 s granularity.
const DEFAULT_WINDOW_SECS: i64 = 60 * 60;
/// Default bucket size when `step` isn't specified. 5 minutes is the
/// MRTG / Cacti convention and lines up with the 60-second sample
/// cadence — 5 samples per bucket gives a smooth average.
const DEFAULT_STEP_SECS: i64 = 5 * 60;
/// Hard ceiling on bucket count to prevent a runaway query (e.g.
/// `from=1y ago&step=1s` → 31 M rows). The SPA's longest range is
/// 30 d at 1 h granularity = 720 buckets, so 10 000 is enough headroom
/// for any reasonable zoom while still cutting off the pathological
/// case.
const MAX_BUCKETS: i64 = 10_000;

#[derive(Deserialize)]
pub struct PerfQuery {
    /// RFC3339 timestamp lower bound (inclusive). Defaults to
    /// `to - 1h`.
    from: Option<DateTime<Utc>>,
    /// RFC3339 timestamp upper bound (exclusive). Defaults to "now".
    to: Option<DateTime<Utc>>,
    /// Bucket size as humantime (e.g. `30s`, `5m`, `1h`). Defaults
    /// to `5m`.
    step: Option<String>,
}

#[derive(Serialize)]
pub struct PerfResponse {
    pub pc_id: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub step_seconds: i64,
    pub points: Vec<PerfPoint>,
}

#[derive(Serialize)]
pub struct PerfPoint {
    pub at: DateTime<Utc>,
    pub cpu_pct: Option<f64>,
    pub mem_used_bytes: Option<f64>,
    pub mem_total_bytes: Option<f64>,
    pub swap_used_bytes: Option<f64>,
    pub swap_total_bytes: Option<f64>,
    pub disk_read_bytes_per_sec: Option<f64>,
    pub disk_written_bytes_per_sec: Option<f64>,
    pub net_rx_bytes_per_sec: Option<f64>,
    pub net_tx_bytes_per_sec: Option<f64>,
}

pub async fn perf(
    State(pool): State<SqlitePool>,
    Path(pc_id): Path<String>,
    Query(q): Query<PerfQuery>,
) -> Result<Json<PerfResponse>, StatusCode> {
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
    if step_secs <= 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if from >= to {
        return Err(StatusCode::BAD_REQUEST);
    }
    let window_secs = (to - from).num_seconds();
    if window_secs / step_secs > MAX_BUCKETS {
        return Err(StatusCode::BAD_REQUEST);
    }

    // SQLite `strftime('%s', at)` returns the row's at as seconds-
    // since-epoch text; the floor-then-multiply gives the bucket
    // boundary's epoch seconds, which we convert back to a
    // DateTime<Utc> on the way out. `CAST(... AS INTEGER)` is needed
    // because strftime returns text.
    let rows = sqlx::query(
        "SELECT
             (CAST(strftime('%s', at) AS INTEGER) / ?) * ? AS bucket_unix,
             AVG(cpu_pct)                    AS cpu_pct,
             AVG(mem_used_bytes)             AS mem_used_bytes,
             AVG(mem_total_bytes)            AS mem_total_bytes,
             AVG(swap_used_bytes)            AS swap_used_bytes,
             AVG(swap_total_bytes)           AS swap_total_bytes,
             AVG(disk_read_bytes_per_sec)    AS disk_read_bytes_per_sec,
             AVG(disk_written_bytes_per_sec) AS disk_written_bytes_per_sec,
             AVG(net_rx_bytes_per_sec)       AS net_rx_bytes_per_sec,
             AVG(net_tx_bytes_per_sec)       AS net_tx_bytes_per_sec
         FROM host_perf_samples
         WHERE pc_id = ?
           AND at >= ?
           AND at < ?
         GROUP BY bucket_unix
         ORDER BY bucket_unix ASC",
    )
    .bind(step_secs)
    .bind(step_secs)
    .bind(&pc_id)
    .bind(from)
    .bind(to)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id, "host_perf query");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let points = rows
        .into_iter()
        .map(|r| {
            let bucket: i64 = r.try_get("bucket_unix").unwrap_or(0);
            PerfPoint {
                at: DateTime::<Utc>::from_timestamp(bucket, 0).unwrap_or(from),
                cpu_pct: r.try_get("cpu_pct").ok(),
                mem_used_bytes: r.try_get("mem_used_bytes").ok(),
                mem_total_bytes: r.try_get("mem_total_bytes").ok(),
                swap_used_bytes: r.try_get("swap_used_bytes").ok(),
                swap_total_bytes: r.try_get("swap_total_bytes").ok(),
                disk_read_bytes_per_sec: r.try_get("disk_read_bytes_per_sec").ok(),
                disk_written_bytes_per_sec: r.try_get("disk_written_bytes_per_sec").ok(),
                net_rx_bytes_per_sec: r.try_get("net_rx_bytes_per_sec").ok(),
                net_tx_bytes_per_sec: r.try_get("net_tx_bytes_per_sec").ok(),
            }
        })
        .collect();

    Ok(Json(PerfResponse {
        pc_id,
        from,
        to,
        step_seconds: step_secs,
        points,
    }))
}
