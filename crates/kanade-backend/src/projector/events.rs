//! v0.30 / PR α' — STREAM_EVENTS consumer that projects
//! `events.started.*.*` payloads into the unified
//! `execution_results` table as in-flight rows (`finished_at = NULL`).
//! The matching ExecResult later UPSERTs against the same
//! `result_id` and flips `finished_at` from NULL to the script's
//! finish timestamp, transitioning the row from "running" to
//! "finished" for any consumer querying via `finished_at IS NULL`.
//!
//! Key design: events.started + ExecResult share `result_id` (the
//! agent mints it once at `handle_command` entry and forwards to
//! both). Backend UPSERTs against `result_id`, so arrival order
//! doesn't matter — same row gets touched both times.
//!
//! Redelivery handling: `ON CONFLICT(result_id) DO NOTHING` —
//! JetStream redelivery of the same started event is idempotent.
//! ExecResult-first race: the events.started arrives later and
//! ON CONFLICT no-ops, leaving the already-finished row untouched.
//! Either way: clean single row, no ghost.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use kanade_shared::kv::STREAM_EVENTS;
use kanade_shared::subject::EVENTS_STARTED_FILTER;
use kanade_shared::wire::EventStarted;
use sqlx::SqlitePool;
use std::time::Duration;
use tracing::{debug, info, warn};

use super::spec_cache::ExplodeSpecCache;

/// Minimum reap slack regardless of how short a job's timeout is — the
/// agent still needs time to force-kill its child, flush its events
/// outbox, and for the strictly-serial projector to land the result.
const MIN_REAP_SLACK: Duration = Duration::from_secs(120);

/// Per-run reap deadline: the instant the backend should stop waiting
/// for an `ExecResult` and let the cleanup reaper give up on the
/// in-flight row. The agent force-kills its child at `execute.timeout`
/// and emits a result, so a row still in-flight past `timeout + slack`
/// means the agent itself died mid-run (or the result was lost), not
/// that the run is slow. `slack = max(2m, timeout/2)` absorbs the
/// agent-kill + outbox-flush + projector lag and gives longer jobs
/// proportionally more grace.
///
/// Computed off `recorded_at` — the JetStream/backend-side publish
/// clock — NOT the agent-stamped `started_at`, so the deadline lives in
/// the same clock domain as the reaper's `now` and is immune to agent
/// clock skew across the fleet (#682).
fn reap_deadline(timeout: Duration, recorded_at: DateTime<Utc>) -> DateTime<Utc> {
    let slack = std::cmp::max(MIN_REAP_SLACK, timeout / 2);
    // Every arm is checked_*: a malformed/absurd manifest timeout (humantime
    // can parse values near Duration::MAX) must not overflow and panic — the
    // events projector runs in a background task, so a panic here halts ALL
    // future projection (gemini/claude #684). On overflow, clamp the span to
    // a 1-year cap (still far past any real run's timeout).
    let cap = chrono::Duration::days(365);
    let span = timeout
        .checked_add(slack)
        .and_then(|d| chrono::Duration::from_std(d).ok())
        .unwrap_or(cap);
    recorded_at
        .checked_add_signed(span)
        .unwrap_or_else(|| recorded_at + cap)
}

/// Resolve a started run's reap deadline from the cached manifest's
/// `execute.timeout`. Returns None when the manifest isn't cached
/// (ad-hoc / not-yet-warmed) or its timeout string won't parse — the
/// reaper then falls back to the flat `INFLIGHT_TIMEOUT_HOURS` cutoff.
async fn resolve_expires_at(
    cache: &ExplodeSpecCache,
    manifest_id: &str,
    recorded_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let manifest = cache.manifest(manifest_id).await?;
    let timeout = humantime::parse_duration(&manifest.execute.timeout).ok()?;
    Some(reap_deadline(timeout, recorded_at))
}

// pub(crate): consumer_reset::reset_if_wiped names this durable when
// deciding what to drop after a projection-DB wipe (#389).
pub(crate) const CONSUMER_NAME: &str = "backend_events_projector";

pub async fn run(js: jetstream::Context, pool: SqlitePool, cache: ExplodeSpecCache) -> Result<()> {
    let stream = js
        .get_stream(STREAM_EVENTS)
        .await
        .with_context(|| format!("get stream {STREAM_EVENTS}"))?;
    let consumer = stream
        .get_or_create_consumer(
            CONSUMER_NAME,
            PullConfig {
                durable_name: Some(CONSUMER_NAME.into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                filter_subject: EVENTS_STARTED_FILTER.into(),
                ..Default::default()
            },
        )
        .await
        .context("create events consumer")?;
    info!(
        stream = STREAM_EVENTS,
        consumer = CONSUMER_NAME,
        filter = EVENTS_STARTED_FILTER,
        "events projector started"
    );

    let mut messages = consumer
        .messages()
        .await
        .context("subscribe events messages")?;
    while let Some(msg) = messages.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "events consumer error");
                continue;
            }
        };
        // #398: recorded_at = the message's JetStream publish time,
        // so a -WipeDb re-projection (#389) reproduces the original
        // arrival times instead of stamping everything "now".
        let recorded_at = super::publish_time(&msg);
        match serde_json::from_slice::<EventStarted>(&msg.payload) {
            Ok(e) => {
                // #682: stamp the per-run reap deadline now (cache hit =
                // no broker round-trip). A miss leaves it NULL and the
                // reaper falls back to the flat 24h cutoff.
                let expires_at = resolve_expires_at(&cache, &e.manifest_id, recorded_at).await;
                if let Err(err) = insert_inflight_row(&pool, &e, recorded_at, expires_at).await {
                    warn!(
                        error = %err,
                        result_id = %e.result_id,
                        exec_id = %e.exec_id,
                        pc_id = %e.pc_id,
                        "events_started insert failed — skipping ack so JetStream redelivers",
                    );
                    // Skip ack so JetStream redelivers. SQLite-busy
                    // / transient errors are recoverable; permanent
                    // errors (schema mismatch) surface via repeated
                    // warn-logs and are operator-visible.
                    continue;
                }
                debug!(
                    result_id = %e.result_id,
                    exec_id = %e.exec_id,
                    pc_id = %e.pc_id,
                    manifest_id = %e.manifest_id,
                    "projected events.started",
                );
                // #682 Stage 2: now that the agent has actually started,
                // advance the parent `executions` row pending → running so
                // the Jobs page live chip matches the Activity/coverage
                // view (both already light up off the execution_results
                // in-flight row this same event just wrote). Best-effort:
                // the in-flight row is the source of truth, and the
                // ExecResult's own bump_exec_counters repairs the status
                // if this transient-fails. See `promote_execution_running`.
                match promote_execution_running(&pool, &e.exec_id).await {
                    Ok(n) if n > 0 => {
                        debug!(exec_id = %e.exec_id, "promoted execution pending→running")
                    }
                    Ok(_) => {}
                    Err(err) => warn!(
                        error = %err,
                        exec_id = %e.exec_id,
                        "promote execution to running failed (non-fatal)",
                    ),
                }
            }
            Err(e) => warn!(
                error = %e,
                subject = %msg.subject,
                "deserialize EventStarted",
            ),
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack events message");
        }
    }
    Ok(())
}

/// Create an in-flight row in `execution_results` (finished_at,
/// exit_code, stdout, stderr all default). `ON CONFLICT(result_id)
/// DO NOTHING` handles both same-direction redelivery (same started
/// event arriving twice) and out-of-order (ExecResult already
/// landed and inserted/updated the row, started's redelivery now
/// no-ops). Either way: one row, no ghost.
async fn insert_inflight_row(
    pool: &SqlitePool,
    e: &EventStarted,
    recorded_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<()> {
    // `execution_results.job_id` (added in migration 0002) holds
    // the manifest id (= cmd id), NOT exec_id — naming legacy from
    // pre-v0.29 when Command.job_id was misnamed and conflated with
    // the deploy UUID. We bind EventStarted.manifest_id to it so
    // the dedup index (job_id, pc_id, finished_at DESC) keeps
    // working unchanged.
    // #390: recorded_at is bound explicitly (RFC 3339) — the column's
    // DEFAULT CURRENT_TIMESTAMP writes space-separated text that
    // breaks lexicographic `recorded_at >= ?` filters. #398: the value
    // is the message's JetStream publish time (see
    // `projector::publish_time`), re-projection-stable by design.
    sqlx::query(
        "INSERT INTO execution_results (
             result_id, request_id, exec_id, pc_id, started_at,
             version, job_id, recorded_at, expires_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(result_id) DO NOTHING",
    )
    .bind(&e.result_id)
    .bind(&e.request_id)
    .bind(&e.exec_id)
    .bind(&e.pc_id)
    .bind(e.started_at)
    .bind(&e.version)
    .bind(&e.manifest_id)
    .bind(recorded_at)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// #682 Stage 2: flip the parent `executions` row pending → running when
/// the agent actually starts (this events.started). Without it the
/// `executions` row only leaves `pending` when the FIRST `ExecResult`
/// lands (`bump_exec_counters`), so a run that takes longer than the 1h
/// `expire_stale_pending` cutoff is wrongly flipped to `expired` while
/// still running — and the Jobs page live chip (which reads
/// `executions.status`) disagrees with the Activity/coverage view (which
/// reads the `execution_results` in-flight row).
///
/// Guarded on `status = 'pending'`: idempotent under JetStream
/// redelivery, and it never clobbers a row a racing result already
/// advanced to running/completed, nor resurrects an `expired` one (a
/// genuinely late result still settles that via `bump_exec_counters`).
/// `exec_id` is empty only for ad-hoc `kanade run` (no executions row) —
/// the UPDATE then matches nothing, a harmless no-op. Returns the rows
/// affected (0 or 1).
async fn promote_execution_running(pool: &SqlitePool, exec_id: &str) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE executions SET status = 'running'
          WHERE exec_id = ? AND status = 'pending'",
    )
    .bind(exec_id)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
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

    fn sample(result_id: &str, exec_id: &str, pc_id: &str) -> EventStarted {
        EventStarted {
            result_id: result_id.into(),
            request_id: "req-1".into(),
            exec_id: exec_id.into(),
            pc_id: pc_id.into(),
            started_at: Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap(),
            manifest_id: "inv-hw".into(),
            version: "1.0.0".into(),
        }
    }

    #[tokio::test]
    async fn events_started_creates_inflight_row() {
        let pool = fresh_pool().await;
        insert_inflight_row(&pool, &sample("r1", "e1", "pc1"), Utc::now(), None)
            .await
            .unwrap();
        let row: (Option<i64>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
            "SELECT exit_code, finished_at FROM execution_results WHERE result_id = ?",
        )
        .bind("r1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, None, "in-flight rows have no exit_code yet");
        assert_eq!(row.1, None, "in-flight rows have no finished_at yet");
    }

    #[tokio::test]
    async fn events_started_redelivery_is_idempotent() {
        // Same event delivered twice (JetStream ack timeout) must
        // not create a second row. `ON CONFLICT(result_id) DO
        // NOTHING` makes the second insert a no-op.
        let pool = fresh_pool().await;
        let e = sample("r1", "e1", "pc1");
        insert_inflight_row(&pool, &e, Utc::now(), None)
            .await
            .unwrap();
        insert_inflight_row(&pool, &e, Utc::now(), None)
            .await
            .unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM execution_results WHERE result_id = ?")
                .bind("r1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn events_started_after_result_does_not_overwrite() {
        // Out-of-order: ExecResult landed first (and either via
        // results projector's UPSERT or a direct INSERT, the row
        // exists with finished_at set). Now events.started arrives
        // (redelivery). ON CONFLICT DO NOTHING leaves the finished
        // row alone — no ghost, no data clobber.
        let pool = fresh_pool().await;
        // Stage the post-finish state: execution_results has the
        // row for r1 with finished_at set + exit_code. job_id
        // column carries the manifest id (legacy v0.19 semantic).
        sqlx::query(
            "INSERT INTO execution_results (
                 result_id, request_id, exec_id, pc_id, exit_code,
                 started_at, finished_at, job_id, version
             ) VALUES (
                 'r1', 'req-1', 'e1', 'pc1', 0,
                 '2026-05-20T12:00:00Z', '2026-05-20T12:00:05Z',
                 'inv-hw', '1.0.0'
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Out-of-order start arrives.
        insert_inflight_row(&pool, &sample("r1", "e1", "pc1"), Utc::now(), None)
            .await
            .unwrap();
        // Row count still 1, finished_at still set.
        let row: (i64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
            "SELECT COUNT(*) AS n, MAX(finished_at) AS f FROM execution_results WHERE result_id = ?",
        )
        .bind("r1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 1);
        assert!(
            row.1.is_some(),
            "finished_at must remain set; events.started must not clobber it",
        );
    }

    #[tokio::test]
    async fn broadcast_fan_out_creates_one_row_per_pc() {
        // Broadcast Command → N PCs each emit events.started with
        // distinct result_id (= per-PC UUID). All N rows persist.
        let pool = fresh_pool().await;
        insert_inflight_row(&pool, &sample("r1", "e1", "pc1"), Utc::now(), None)
            .await
            .unwrap();
        insert_inflight_row(&pool, &sample("r2", "e1", "pc2"), Utc::now(), None)
            .await
            .unwrap();
        insert_inflight_row(&pool, &sample("r3", "e1", "pc3"), Utc::now(), None)
            .await
            .unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM execution_results WHERE exec_id = ?")
                .bind("e1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 3);
    }

    #[tokio::test]
    async fn insert_inflight_row_persists_expires_at() {
        // #682: when the projector resolves a deadline it must land on
        // the row so the reaper can key on it.
        let pool = fresh_pool().await;
        let recorded = Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap();
        let expires = reap_deadline(Duration::from_secs(360), recorded);
        insert_inflight_row(&pool, &sample("r1", "e1", "pc1"), recorded, Some(expires))
            .await
            .unwrap();
        let row: (Option<DateTime<Utc>>,) =
            sqlx::query_as("SELECT expires_at FROM execution_results WHERE result_id = ?")
                .bind("r1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, Some(expires));
    }

    #[test]
    fn reap_deadline_uses_min_slack_for_short_timeouts() {
        // A 20s check: slack floors at MIN_REAP_SLACK (2m), not 10s.
        let recorded = Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap();
        let d = reap_deadline(Duration::from_secs(20), recorded);
        // 20s timeout + 120s floor slack = 140s.
        assert_eq!(d, recorded + chrono::Duration::seconds(140));
    }

    #[test]
    fn reap_deadline_scales_slack_for_long_timeouts() {
        // A 1h timeout: slack = timeout/2 = 30m, so deadline = +90m.
        let recorded = Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap();
        let d = reap_deadline(Duration::from_secs(3600), recorded);
        assert_eq!(d, recorded + chrono::Duration::minutes(90));
    }

    async fn insert_execution(pool: &SqlitePool, exec_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO executions
                (exec_id, job_id, version, initiated_by, target_count, status)
             VALUES (?, 'j', '1.0', 'tester', 1, ?)",
        )
        .bind(exec_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn exec_status(pool: &SqlitePool, exec_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM executions WHERE exec_id = ?")
            .bind(exec_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn promote_execution_running_flips_pending() {
        // #682 Stage 2: events.started advances the parent execution
        // pending → running the moment the agent starts.
        let pool = fresh_pool().await;
        insert_execution(&pool, "e1", "pending").await;
        let n = promote_execution_running(&pool, "e1").await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(exec_status(&pool, "e1").await, "running");
    }

    #[tokio::test]
    async fn promote_execution_running_is_idempotent_and_safe() {
        // Re-running (redelivery) is a no-op; and it never resurrects a
        // completed or expired row, nor errors on a missing exec_id.
        let pool = fresh_pool().await;
        insert_execution(&pool, "done", "completed").await;
        insert_execution(&pool, "gone", "expired").await;
        assert_eq!(promote_execution_running(&pool, "done").await.unwrap(), 0);
        assert_eq!(promote_execution_running(&pool, "gone").await.unwrap(), 0);
        assert_eq!(exec_status(&pool, "done").await, "completed");
        assert_eq!(exec_status(&pool, "gone").await, "expired");
        // Missing exec_id (ad-hoc run, no executions row) → harmless no-op.
        assert_eq!(promote_execution_running(&pool, "nope").await.unwrap(), 0);
    }
}
