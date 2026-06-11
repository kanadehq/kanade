//! `GET /api/health/fleet` — machine-readable rollup of fleet
//! health for external monitors (Nagios, Prometheus blackbox,
//! k8s liveness probes, alerting cron).
//!
//! Returns JSON describing the agent inventory freshness, the
//! JetStream resource set, and recent execution failures. HTTP
//! status mirrors `status`:
//!
//!   * `ok`        → 200 — all resources present, no stale agents
//!   * `unknown`   → 200 — no agents reporting yet (fresh install)
//!   * `degraded`  → 503 — at least one JetStream resource missing
//!     OR one or more agents have gone stale
//!
//! The endpoint sits under `/api/*` so it inherits the same auth
//! middleware as the rest of the admin surface; if your monitor
//! can't carry a bearer token, hit the unauthenticated `/health`
//! liveness probe instead.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_SCHEDULES,
    BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, STREAM_AUDIT,
    STREAM_EVENTS, STREAM_EXEC, STREAM_INVENTORY, STREAM_RESULTS,
};
use serde::Serialize;
use sqlx::Row;
use tracing::warn;

use super::AppState;

/// Stale threshold for `last_heartbeat`. Heartbeats cadence at 30 s
/// by default; 2 min of slack catches a few missed ticks without
/// flapping during a single packet drop. Matches the Dashboard's
/// "active" rollup.
const STALE_THRESHOLD: Duration = Duration::minutes(2);

/// Look-back window for the recent-failure rollup.
const RECENT_WINDOW: Duration = Duration::hours(24);

#[derive(Serialize)]
pub struct FleetHealth {
    pub status: HealthStatus,
    pub agents: AgentsHealth,
    pub jetstream: JetstreamHealth,
    pub recent_results: RecentResults,
    pub observed_at: DateTime<Utc>,
}

#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Ok,
    Unknown,
    Degraded,
}

#[derive(Serialize)]
pub struct AgentsHealth {
    pub known: i64,
    pub active: i64,
    pub stale: i64,
}

#[derive(Serialize)]
pub struct JetstreamHealth {
    pub all_ok: bool,
    pub healthy: usize,
    pub total: usize,
    pub missing: Vec<String>,
}

#[derive(Serialize)]
pub struct RecentResults {
    pub window_hours: i64,
    pub total: i64,
    pub failed: i64,
}

pub async fn fleet(State(state): State<AppState>) -> (StatusCode, Json<FleetHealth>) {
    let now = Utc::now();
    let stale_cutoff = now - STALE_THRESHOLD;
    let recent_cutoff = now - RECENT_WINDOW;

    let agents = agents_health(&state.pool, stale_cutoff).await;
    let jetstream = jetstream_health(&state.jetstream).await;
    let recent_results = recent_results(&state.pool, recent_cutoff).await;

    let status = if jetstream.all_ok && agents.stale == 0 && agents.known > 0 {
        HealthStatus::Ok
    } else if !jetstream.all_ok || agents.stale > 0 {
        HealthStatus::Degraded
    } else {
        HealthStatus::Unknown
    };

    let code = match status {
        HealthStatus::Ok | HealthStatus::Unknown => StatusCode::OK,
        HealthStatus::Degraded => StatusCode::SERVICE_UNAVAILABLE,
    };

    (
        code,
        Json(FleetHealth {
            status,
            agents,
            jetstream,
            recent_results,
            observed_at: now,
        }),
    )
}

async fn agents_health(pool: &sqlx::SqlitePool, stale_cutoff: DateTime<Utc>) -> AgentsHealth {
    let row = sqlx::query(
        "SELECT
             COUNT(*) AS known,
             COALESCE(SUM(CASE WHEN last_heartbeat >= ? THEN 1 ELSE 0 END), 0) AS active
         FROM agents",
    )
    .bind(stale_cutoff)
    .fetch_one(pool)
    .await;
    let (known, active) = match row {
        Ok(r) => (
            r.try_get::<i64, _>("known").unwrap_or(0),
            r.try_get::<i64, _>("active").unwrap_or(0),
        ),
        Err(e) => {
            warn!(error = %e, "agents_health query");
            (0, 0)
        }
    };
    AgentsHealth {
        known,
        active,
        stale: (known - active).max(0),
    }
}

async fn jetstream_health(js: &async_nats::jetstream::Context) -> JetstreamHealth {
    let mut missing = Vec::new();
    let mut total = 0usize;

    for name in [
        STREAM_INVENTORY,
        STREAM_RESULTS,
        STREAM_EXEC,
        STREAM_EVENTS,
        STREAM_AUDIT,
    ] {
        total += 1;
        if js.get_stream(name).await.is_err() {
            missing.push(name.to_string());
        }
    }
    for name in [
        BUCKET_SCRIPT_CURRENT,
        BUCKET_SCRIPT_STATUS,
        BUCKET_AGENTS_STATE,
        BUCKET_AGENT_CONFIG,
        BUCKET_AGENT_GROUPS,
        BUCKET_SCHEDULES,
    ] {
        total += 1;
        if js.get_key_value(name).await.is_err() {
            missing.push(name.to_string());
        }
    }
    for name in [OBJECT_AGENT_RELEASES] {
        total += 1;
        if js.get_object_store(name).await.is_err() {
            missing.push(name.to_string());
        }
    }

    JetstreamHealth {
        all_ok: missing.is_empty(),
        healthy: total - missing.len(),
        total,
        missing,
    }
}

async fn recent_results(pool: &sqlx::SqlitePool, since: DateTime<Utc>) -> RecentResults {
    let row = sqlx::query(
        "SELECT
             COUNT(*) AS total,
             COALESCE(SUM(CASE WHEN exit_code <> 0 THEN 1 ELSE 0 END), 0) AS failed
         FROM execution_results
         WHERE recorded_at >= ?",
    )
    .bind(since)
    .fetch_one(pool)
    .await;
    let (total, failed) = match row {
        Ok(r) => (
            r.try_get::<i64, _>("total").unwrap_or(0),
            r.try_get::<i64, _>("failed").unwrap_or(0),
        ),
        Err(e) => {
            warn!(error = %e, "recent_results query");
            (0, 0)
        }
    };
    RecentResults {
        window_hours: RECENT_WINDOW.num_hours(),
        total,
        failed,
    }
}

/// v0.36 follow-up: per-job scan-duration aggregates over a recent
/// window. Drives a small "are my probes healthy?" panel without
/// any agent-side instrumentation — `execution_results.started_at`
/// and `finished_at` are already populated for every finished row,
/// so duration = `finished_at - started_at` and aggregating is a
/// pure-SQL + Rust-side percentile reduction.
///
/// One row per `job_id` that has at least one finished result in
/// the window. Sorted by p95 descending so the slowest probes
/// surface at the top.
#[derive(Serialize)]
pub struct ScanDurationStats {
    pub job_id: String,
    pub count: i64,
    pub min_ms: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub p99_ms: i64,
    pub max_ms: i64,
    pub mean_ms: i64,
    /// `result_id` of the single slowest finished run in the window for
    /// this job_id — lets the dashboard's "max" cell deep-link straight
    /// to that run's detail page. `None` only when the slowest row had
    /// no `result_id` (shouldn't happen for a finished row).
    pub max_result_id: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct ScanDurationParams {
    /// Lower bound on `finished_at`. ISO-8601 / RFC 3339. Defaults
    /// to "24h ago" when omitted so the panel has a sensible
    /// default without the operator having to think. We filter on
    /// `finished_at` (not `started_at`) because (a) it is served by
    /// the single-column `idx_execution_results_finished_at` index
    /// (#516), and (b) the natural question is "what finished in
    /// this window", so a long-running scan that started before the
    /// window but finished inside it still belongs in the
    /// percentile bucket.
    pub since: Option<DateTime<Utc>>,
}

/// SQLite has no native PERCENTILE_* — pull durations grouped by
/// job_id, sort each group, index by rank. Cheap for the volumes
/// kanade fleets generate (3000 PCs × 6 manifests × 24 h / 6 h
/// cooldown ≈ 12000 finished rows, plenty for in-memory sort per
/// job_id bucket).
pub async fn scan_durations(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ScanDurationParams>,
) -> Result<Json<Vec<ScanDurationStats>>, (StatusCode, String)> {
    let since = params
        .since
        .unwrap_or_else(|| Utc::now() - Duration::hours(24));

    // Filter on `finished_at >= ?` — semantically correct for a
    // "what completed in this window" rollup: a 25-h scan that
    // started 25 h ago and finished inside the window belongs in
    // the percentile bucket; a `started_at` predicate would exclude
    // it. The bare range is served by the single-column
    // idx_execution_results_finished_at index (#516) — the
    // composite (job_id, pc_id, finished_at DESC) index CANNOT
    // serve it (finished_at is the third column; leading-column
    // rule), which is why this query used to full-scan.
    let rows = sqlx::query(
        "SELECT job_id, result_id, \
                CAST((julianday(finished_at) - julianday(started_at)) * 86400000.0 AS INTEGER) AS dur_ms \
           FROM execution_results \
          WHERE finished_at IS NOT NULL \
            AND started_at IS NOT NULL \
            AND job_id IS NOT NULL \
            AND finished_at >= ?",
    )
    .bind(since)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "scan_durations query");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Bucket per job_id as (duration, result_id) pairs, sort each by
    // duration, compute percentiles — and the slowest row's result_id
    // for the dashboard's "max" deep-link.
    let mut by_job: std::collections::HashMap<String, Vec<(i64, Option<String>)>> =
        std::collections::HashMap::new();
    for r in &rows {
        let Ok(job_id) = r.try_get::<String, _>("job_id") else {
            continue;
        };
        // SQLite returns the CAST as i64. Clamp to >= 0 so a
        // clock-skew row where finished_at < started_at can't
        // produce a negative duration entry. (Shouldn't happen on
        // a single agent, but cross-projector-restart edge cases
        // and NTP rewinds exist.)
        let dur = r.try_get::<i64, _>("dur_ms").unwrap_or(0).max(0);
        let result_id = r.try_get::<String, _>("result_id").ok();
        by_job.entry(job_id).or_default().push((dur, result_id));
    }
    let mut out: Vec<ScanDurationStats> = by_job
        .into_iter()
        .map(|(job_id, mut pairs)| {
            // Sort by duration so percentiles index by rank and the
            // slowest run (carrying the deep-link result_id) is last.
            pairs.sort_unstable_by_key(|(dur, _)| *dur);
            let durs: Vec<i64> = pairs.iter().map(|(dur, _)| *dur).collect();
            let count = durs.len() as i64;
            let sum: i64 = durs.iter().sum();
            let mean_ms = if count > 0 { sum / count } else { 0 };
            let max_result_id = pairs.last().and_then(|(_, rid)| rid.clone());
            ScanDurationStats {
                job_id,
                count,
                min_ms: *durs.first().unwrap_or(&0),
                p50_ms: percentile(&durs, 0.50),
                p95_ms: percentile(&durs, 0.95),
                p99_ms: percentile(&durs, 0.99),
                max_ms: *durs.last().unwrap_or(&0),
                mean_ms,
                max_result_id,
            }
        })
        .collect();
    // Slowest at the top — the operator's first question is "which
    // probe is hurting", not "what's the alphabetical first".
    out.sort_by_key(|s| std::cmp::Reverse(s.p95_ms));
    Ok(Json(out))
}

/// Nearest-rank percentile on a pre-sorted slice. Empty slice → 0.
/// q in [0.0, 1.0] — caller's responsibility to pass a sensible
/// value; clamped defensively here.
fn percentile(sorted: &[i64], q: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let q = q.clamp(0.0, 1.0);
    // Rank index: nearest-rank (ceil) so p95 of a 20-element vec
    // picks the 19th value (index 18), not the 19th value as
    // interpolated. Matches what most operator-facing tools do
    // (prometheus quantile, datadog, etc.) closely enough for
    // a "is my probe slow?" panel.
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[], 0.95), 0);
    }

    #[test]
    fn percentile_single_element() {
        assert_eq!(percentile(&[42], 0.0), 42);
        assert_eq!(percentile(&[42], 0.5), 42);
        assert_eq!(percentile(&[42], 1.0), 42);
    }

    #[test]
    fn percentile_nearest_rank_on_ten_elements() {
        let v: Vec<i64> = (1..=10).collect(); // [1..=10]
        // p50 — rank ceil(0.50*10)=5, index 4, value 5
        assert_eq!(percentile(&v, 0.50), 5);
        // p90 — rank ceil(0.90*10)=9, index 8, value 9
        assert_eq!(percentile(&v, 0.90), 9);
        // p95 — rank ceil(0.95*10)=10, index 9, value 10
        assert_eq!(percentile(&v, 0.95), 10);
        // p99 — rank ceil(0.99*10)=10, index 9, value 10
        assert_eq!(percentile(&v, 0.99), 10);
    }

    #[test]
    fn percentile_q_clamps_to_unit_interval() {
        let v: Vec<i64> = (1..=10).collect();
        // Negative q clamps to 0 → idx 0 → value 1.
        assert_eq!(percentile(&v, -0.5), 1);
        // q > 1 clamps to 1 → idx 9 → value 10.
        assert_eq!(percentile(&v, 1.5), 10);
    }
}
