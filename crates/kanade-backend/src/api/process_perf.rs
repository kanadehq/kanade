//! `/api/agents/{pc_id}/processes` — most recent per-process snapshot.
//!
//! V1 surface is intentionally minimal: returns the top-N processes
//! from the **latest** tick the projector has on file for this PC.
//! No time-range slicing yet — the operator use case is "what's
//! pegging this host right now?", so a snapshot is more useful than
//! a timeline at this stage.
//!
//! [`timeline`] (sibling endpoint) is the chart-driving counterpart:
//! same table, but bucketed in SQL with the top-N processes pinned
//! **across the whole window** so a Recharts stacked area can colour
//! each name consistently bucket-to-bucket. Anything outside that
//! top-N is rolled into a single `other` series so the area total
//! still reflects the full picture.
//!
//! When no samples exist yet (process_perf was never enabled, or the
//! 7-day retention has aged them all out), the response carries an
//! empty `processes` array and `latest_at = null` — the SPA renders
//! that as "no samples in window yet" instead of a 404, because the
//! PC itself is still a valid agent.

use std::collections::HashMap;
use std::str::FromStr;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct ProcessesResponse {
    pub pc_id: String,
    pub latest_at: Option<DateTime<Utc>>,
    pub processes: Vec<ProcessRow>,
}

#[derive(Serialize)]
pub struct ProcessRow {
    pub pid: i64,
    pub name: String,
    pub cpu_pct: f64,
    pub rss_bytes: i64,
    pub disk_read_bytes_per_sec: Option<f64>,
    pub disk_written_bytes_per_sec: Option<f64>,
}

pub async fn processes(
    State(pool): State<SqlitePool>,
    Path(pc_id): Path<String>,
) -> Result<Json<ProcessesResponse>, StatusCode> {
    // Two-step: pin the latest `at` first, then SELECT the row set
    // belonging to that tick. SQLite's planner handles this just
    // fine without a CTE, and it makes the empty-table branch (no
    // samples yet) easy to detect without parsing back a NULL `at`.
    let latest_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(at) FROM process_perf_samples WHERE pc_id = ?")
            .bind(&pc_id)
            .fetch_one(&pool)
            .await
            .map_err(|e| {
                warn!(error = %e, pc_id, "process_perf latest_at query");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    let Some(at) = latest_at else {
        return Ok(Json(ProcessesResponse {
            pc_id,
            latest_at: None,
            processes: vec![],
        }));
    };

    let rows = sqlx::query(
        "SELECT pid, name, cpu_pct, rss_bytes,
                disk_read_bytes_per_sec, disk_written_bytes_per_sec
         FROM process_perf_samples
         WHERE pc_id = ? AND at = ?
         ORDER BY cpu_pct DESC",
    )
    .bind(&pc_id)
    .bind(at)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id, "process_perf rows query");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let processes = rows
        .into_iter()
        .map(|r| ProcessRow {
            pid: r.try_get("pid").unwrap_or(0),
            name: r.try_get("name").unwrap_or_default(),
            cpu_pct: r.try_get("cpu_pct").unwrap_or(0.0),
            rss_bytes: r.try_get("rss_bytes").unwrap_or(0),
            disk_read_bytes_per_sec: r.try_get("disk_read_bytes_per_sec").ok(),
            disk_written_bytes_per_sec: r.try_get("disk_written_bytes_per_sec").ok(),
        })
        .collect();

    Ok(Json(ProcessesResponse {
        pc_id,
        latest_at: Some(at),
        processes,
    }))
}

// ----- /api/agents/{pc_id}/processes/timeline -----

/// One-hour default window — same convention as the host_perf endpoint.
/// Process-perf retention is only 7 days so callers can't reasonably
/// ask for much beyond that anyway.
const TIMELINE_DEFAULT_WINDOW_SECS: i64 = 60 * 60;
/// 1-min default bucket. Agent ticks publish at the host_perf_interval
/// cadence (default 60 s), so a 1-min bucket = ~1 sample per bucket
/// per name — finest resolution the data supports without empty cells
/// dominating.
const TIMELINE_DEFAULT_STEP_SECS: i64 = 60;
/// Hard ceiling on bucket count × top-N rows so a runaway query stays
/// bounded. 10 000 buckets × 20 names + small constant = ~200k rows
/// worst-case which SQLite handles comfortably.
const TIMELINE_MAX_BUCKETS: i64 = 10_000;
/// Default top-N. Five names + an `other` row reads cleanly in a
/// stacked area without the legend taking over the chart.
const TIMELINE_DEFAULT_TOP: i64 = 5;
/// Max top-N. Past ~20 the colour palette wraps and the chart
/// stops being a comprehension tool; cap to keep the SPA honest.
const TIMELINE_MAX_TOP: i64 = 20;
/// Label assigned to the rolled-up tail of processes that didn't
/// make the window-wide top-N. Stable across responses so the SPA
/// can pin it to a fixed colour.
const OTHER_LABEL: &str = "other";

/// Metric column whitelist. SQLite can't parameterise column names,
/// so the column is spliced as a literal — keeping the source
/// inside a closed enum match makes that splice injection-safe.
/// Local to this module (distinct from [`fleet_perf::Metric`]) because
/// process_perf_samples doesn't carry mem/net columns and offering
/// them in the API would 500 on every request.
#[derive(Clone, Copy, Debug)]
enum TimelineMetric {
    CpuPct,
    RssBytes,
    DiskReadBytesPerSec,
    DiskWrittenBytesPerSec,
}

impl TimelineMetric {
    fn column(self) -> &'static str {
        match self {
            Self::CpuPct => "cpu_pct",
            Self::RssBytes => "rss_bytes",
            Self::DiskReadBytesPerSec => "disk_read_bytes_per_sec",
            Self::DiskWrittenBytesPerSec => "disk_written_bytes_per_sec",
        }
    }
}

impl FromStr for TimelineMetric {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cpu_pct" | "cpu" => Ok(Self::CpuPct),
            "rss_bytes" | "rss" | "mem" | "memory" => Ok(Self::RssBytes),
            "disk_read_bytes_per_sec" | "disk_read" => Ok(Self::DiskReadBytesPerSec),
            "disk_written_bytes_per_sec" | "disk_written" => Ok(Self::DiskWrittenBytesPerSec),
            _ => Err(()),
        }
    }
}

#[derive(Deserialize)]
pub struct TimelineQuery {
    /// Metric to project onto the y-axis. `cpu` / `rss` / `disk_read` /
    /// `disk_written` (plus the underscored column-name forms).
    /// Defaults to `cpu_pct` if absent.
    metric: Option<String>,
    /// RFC3339 lower bound (inclusive). Defaults to `to - 1h`.
    from: Option<DateTime<Utc>>,
    /// RFC3339 upper bound (exclusive). Defaults to "now".
    to: Option<DateTime<Utc>>,
    /// Bucket size as humantime (`30s`, `1m`, `5m`, …). Defaults to
    /// `1m`.
    step: Option<String>,
    /// Top-N process names to surface as their own series. Anything
    /// outside the top-N is collapsed into `other`. Defaults to 5,
    /// clamped to `[1, 20]`.
    top: Option<i64>,
}

#[derive(Serialize)]
pub struct TimelineResponse {
    pub pc_id: String,
    pub metric: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub step_seconds: i64,
    /// Stable display order for the SPA: the window-wide top-N names
    /// (descending by AVG of `metric`) followed by `other` if any tail
    /// rows were collapsed. Empty if the window has no samples at all.
    pub names: Vec<String>,
    pub points: Vec<TimelinePoint>,
}

#[derive(Serialize)]
pub struct TimelinePoint {
    pub at: DateTime<Utc>,
    /// `name → bucket-averaged metric value`. A name absent from this
    /// map for a given bucket means the process wasn't observed there
    /// (sysinfo top-N didn't reach it that tick) — the SPA should treat
    /// missing as 0 when stacking, not as a gap.
    pub values: HashMap<String, f64>,
}

pub async fn timeline(
    State(pool): State<SqlitePool>,
    Path(pc_id): Path<String>,
    Query(q): Query<TimelineQuery>,
) -> Result<Json<TimelineResponse>, StatusCode> {
    let metric = TimelineMetric::from_str(q.metric.as_deref().unwrap_or("cpu_pct"))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let to = q.to.unwrap_or_else(Utc::now);
    let from = q
        .from
        .unwrap_or_else(|| to - Duration::seconds(TIMELINE_DEFAULT_WINDOW_SECS));
    let step_secs = match q.step.as_deref() {
        None => TIMELINE_DEFAULT_STEP_SECS,
        Some(raw) => match humantime::parse_duration(raw) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(TIMELINE_DEFAULT_STEP_SECS),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        },
    };
    if step_secs <= 0 || from >= to {
        return Err(StatusCode::BAD_REQUEST);
    }
    if (to - from).num_seconds() / step_secs > TIMELINE_MAX_BUCKETS {
        return Err(StatusCode::BAD_REQUEST);
    }
    let top_n = q
        .top
        .unwrap_or(TIMELINE_DEFAULT_TOP)
        .clamp(1, TIMELINE_MAX_TOP);

    // Step 1: pin the top-N names across the **whole window**. Doing
    // this server-side (not per-bucket) is what makes the SPA's stacked
    // area readable — series stay the same colour edge-to-edge. The
    // `metric.column()` splice is enum-bounded above, so it's safe.
    let column = metric.column();
    let top_sql = format!(
        "SELECT name
         FROM process_perf_samples
         WHERE pc_id = ? AND at >= ? AND at < ? AND {column} IS NOT NULL
         GROUP BY name
         ORDER BY AVG({column}) DESC NULLS LAST
         LIMIT ?",
    );
    let top_rows = sqlx::query(sqlx::AssertSqlSafe(top_sql))
        .bind(&pc_id)
        .bind(from)
        .bind(to)
        .bind(top_n)
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, pc_id, "process_perf timeline top-names");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let top_names: Vec<String> = top_rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("name").unwrap_or_default())
        .collect();

    // No samples at all in the window → return an empty timeline.
    // Distinct from "samples exist but none have a non-null value" —
    // we don't disambiguate here; the SPA renders both as "no data".
    if top_names.is_empty() {
        return Ok(Json(TimelineResponse {
            pc_id,
            metric: column.to_string(),
            from,
            to,
            step_seconds: step_secs,
            names: vec![],
            points: vec![],
        }));
    }

    // Step 2: bucket every sample in the window, bucketing the *name*
    // too — rows whose name isn't in `top_names` collapse into a
    // single `other` series. Using a CASE over a comma-separated VALUES
    // table sidesteps needing to bind a variable-length IN-list across
    // sqlx's typed binder.
    let mut bucket_sql = String::from(
        "SELECT
             (CAST(strftime('%s', at) AS INTEGER) / ?) * ? AS bucket_unix,
             CASE name ",
    );
    for _ in &top_names {
        bucket_sql.push_str("WHEN ? THEN name ");
    }
    bucket_sql.push_str("ELSE ? END AS bucket_name, AVG(");
    bucket_sql.push_str(column);
    bucket_sql.push_str(
        ") AS value
         FROM process_perf_samples
         WHERE pc_id = ? AND at >= ? AND at < ?
         GROUP BY bucket_unix, bucket_name
         ORDER BY bucket_unix ASC",
    );

    let mut q = sqlx::query(sqlx::AssertSqlSafe(bucket_sql))
        .bind(step_secs)
        .bind(step_secs);
    for n in &top_names {
        q = q.bind(n);
    }
    let rows = q
        .bind(OTHER_LABEL)
        .bind(&pc_id)
        .bind(from)
        .bind(to)
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, pc_id, "process_perf timeline buckets");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Fold flat (bucket, name, value) rows into one TimelinePoint per
    // bucket. BTreeMap-on-the-way-in would keep buckets sorted, but the
    // SQL already returns them sorted; use a Vec + linear-merge so we
    // preserve order without a second sort pass.
    let mut points: Vec<TimelinePoint> = Vec::new();
    let mut has_other = false;
    for r in rows {
        let bucket: i64 = r.try_get("bucket_unix").unwrap_or(0);
        let name: String = r.try_get("bucket_name").unwrap_or_default();
        let value: Option<f64> = r.try_get("value").ok();
        let Some(v) = value else { continue };
        if name == OTHER_LABEL {
            has_other = true;
        }
        let at = DateTime::<Utc>::from_timestamp(bucket, 0).unwrap_or(from);
        match points.last_mut() {
            Some(p) if p.at == at => {
                p.values.insert(name, v);
            }
            _ => {
                let mut values = HashMap::new();
                values.insert(name, v);
                points.push(TimelinePoint { at, values });
            }
        }
    }

    // Display order: window-wide top-N (already sorted DESC by AVG),
    // then `other` last so its band sits at the top of the stack in
    // Recharts' default render order. Only include `other` if at least
    // one bucket actually produced a value for it.
    let mut names = top_names;
    if has_other {
        names.push(OTHER_LABEL.to_string());
    }

    Ok(Json(TimelineResponse {
        pc_id,
        metric: column.to_string(),
        from,
        to,
        step_seconds: step_secs,
        names,
        points,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn empty_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn insert_sample(
        pool: &SqlitePool,
        pc_id: &str,
        at: DateTime<Utc>,
        pid: i64,
        name: &str,
        cpu_pct: f64,
        rss_bytes: i64,
    ) {
        sqlx::query(
            "INSERT INTO process_perf_samples (pc_id, at, pid, name, cpu_pct, rss_bytes)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(pc_id)
        .bind(at)
        .bind(pid)
        .bind(name)
        .bind(cpu_pct)
        .bind(rss_bytes)
        .execute(pool)
        .await
        .unwrap();
    }

    fn t(min: i32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 24, 1, min as u32, 0).unwrap()
    }

    #[tokio::test]
    async fn timeline_empty_when_no_samples() {
        let pool = empty_pool().await;
        let resp = timeline(
            State(pool),
            Path("nobody".into()),
            Query(TimelineQuery {
                metric: None,
                from: Some(t(0)),
                to: Some(t(10)),
                step: Some("1m".into()),
                top: None,
            }),
        )
        .await
        .unwrap();
        assert!(resp.names.is_empty());
        assert!(resp.points.is_empty());
        assert_eq!(resp.metric, "cpu_pct");
    }

    #[tokio::test]
    async fn timeline_pins_top_names_across_window_and_collapses_other() {
        let pool = empty_pool().await;
        // bucket 0 (t=0): chrome=80, defender=40, idle=5, tail=1
        insert_sample(&pool, "pc1", t(0), 1, "chrome.exe", 80.0, 1000).await;
        insert_sample(&pool, "pc1", t(0), 2, "defender.exe", 40.0, 500).await;
        insert_sample(&pool, "pc1", t(0), 3, "idle.exe", 5.0, 100).await;
        insert_sample(&pool, "pc1", t(0), 4, "tail.exe", 1.0, 50).await;
        // bucket 1 (t=1): chrome=60, defender=50, idle=2, tail=0
        insert_sample(&pool, "pc1", t(1), 1, "chrome.exe", 60.0, 1100).await;
        insert_sample(&pool, "pc1", t(1), 2, "defender.exe", 50.0, 600).await;
        insert_sample(&pool, "pc1", t(1), 3, "idle.exe", 2.0, 120).await;
        insert_sample(&pool, "pc1", t(1), 4, "tail.exe", 0.5, 50).await;

        let resp = timeline(
            State(pool),
            Path("pc1".into()),
            Query(TimelineQuery {
                metric: Some("cpu".into()),
                from: Some(t(0)),
                to: Some(t(5)),
                step: Some("1m".into()),
                top: Some(2),
            }),
        )
        .await
        .unwrap();

        // top-2 window-wide is chrome (avg 70) + defender (avg 45);
        // idle + tail collapse into `other`.
        assert_eq!(resp.names, vec!["chrome.exe", "defender.exe", "other"]);
        assert_eq!(resp.points.len(), 2);

        let b0 = &resp.points[0];
        assert_eq!(b0.at, t(0));
        assert!((b0.values["chrome.exe"] - 80.0).abs() < 1e-6);
        assert!((b0.values["defender.exe"] - 40.0).abs() < 1e-6);
        // idle (5) + tail (1) → AVG = 3.0
        assert!((b0.values["other"] - 3.0).abs() < 1e-6);

        let b1 = &resp.points[1];
        assert_eq!(b1.at, t(1));
        assert!((b1.values["chrome.exe"] - 60.0).abs() < 1e-6);
        assert!((b1.values["defender.exe"] - 50.0).abs() < 1e-6);
        // idle (2) + tail (0.5) → AVG = 1.25
        assert!((b1.values["other"] - 1.25).abs() < 1e-6);
    }

    #[tokio::test]
    async fn timeline_metric_alias_disk_read_picks_disk_read_column() {
        let pool = empty_pool().await;
        sqlx::query(
            "INSERT INTO process_perf_samples
                (pc_id, at, pid, name, cpu_pct, rss_bytes,
                 disk_read_bytes_per_sec, disk_written_bytes_per_sec)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("pc1")
        .bind(t(0))
        .bind(1_i64)
        .bind("io.exe")
        .bind(1.0_f64)
        .bind(1000_i64)
        .bind(2048.0_f64)
        .bind(0.0_f64)
        .execute(&pool)
        .await
        .unwrap();

        let resp = timeline(
            State(pool),
            Path("pc1".into()),
            Query(TimelineQuery {
                metric: Some("disk_read".into()),
                from: Some(t(0)),
                to: Some(t(5)),
                step: Some("1m".into()),
                top: Some(5),
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.metric, "disk_read_bytes_per_sec");
        assert_eq!(resp.names, vec!["io.exe"]);
        assert!((resp.points[0].values["io.exe"] - 2048.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn timeline_no_other_when_everything_fits_top_n() {
        let pool = empty_pool().await;
        insert_sample(&pool, "pc1", t(0), 1, "a.exe", 50.0, 100).await;
        insert_sample(&pool, "pc1", t(0), 2, "b.exe", 30.0, 200).await;
        let resp = timeline(
            State(pool),
            Path("pc1".into()),
            Query(TimelineQuery {
                metric: None,
                from: Some(t(0)),
                to: Some(t(5)),
                step: Some("1m".into()),
                top: Some(5),
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.names, vec!["a.exe", "b.exe"]);
    }

    #[tokio::test]
    async fn timeline_rejects_bad_metric_and_inverted_range() {
        let pool = empty_pool().await;
        let bad_metric = timeline(
            State(pool.clone()),
            Path("pc1".into()),
            Query(TimelineQuery {
                metric: Some("nope".into()),
                from: Some(t(0)),
                to: Some(t(1)),
                step: None,
                top: None,
            }),
        )
        .await;
        assert_eq!(bad_metric.err(), Some(StatusCode::BAD_REQUEST));

        let inverted = timeline(
            State(pool),
            Path("pc1".into()),
            Query(TimelineQuery {
                metric: None,
                from: Some(t(5)),
                to: Some(t(0)),
                step: None,
                top: None,
            }),
        )
        .await;
        assert_eq!(inverted.err(), Some(StatusCode::BAD_REQUEST));
    }
}
