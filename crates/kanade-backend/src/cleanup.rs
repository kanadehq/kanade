//! v0.30.0 follow-up: periodic housekeeping that marks long-stale
//! `executions` rows as `expired`. Without this, `status = 'pending'`
//! rows accumulate forever — every fire whose ExecResult never
//! lands (offline target PCs, `run_as: user` with no session, agent
//! died mid-script, deadline-missed before a result was emitted)
//! leaves a permanent entry that the Jobs page's live chip counts.
//!
//! Operator-observable symptom that triggered this: `pending: 111`
//! on a `run_as: user` job whose target PC didn't have a console
//! session most of the time, so most fires never produced an
//! ExecResult and the projector never got to transition the row.
//!
//! Policy:
//!   * `pending` older than `PENDING_TIMEOUT_HOURS` → `expired`.
//!     Most deployments transition to `running` within seconds of
//!     fire (first ExecResult arrival), so 1 h is generously long.
//!   * `running` rows are left alone — they have at least one
//!     result, so they're "partially observed" rather than
//!     "abandoned". Operator can investigate via the
//!     `/api/executions/{exec_id}` detail view if a partial fan-out
//!     concerns them.
//!
//! The Jobs page live chip queries `status IN ('pending', 'running')`,
//! so once a stale row flips to `expired` it falls out of the chip
//! naturally — no SPA filter change needed.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tracing::{info, warn};

// #390 note on cutoff style: every sweep below takes a chrono-bound
// `cutoff` computed in the tick loop instead of SQLite-side
// `datetime('now', '-N hours')`. The columns being compared store
// RFC 3339 text ('2026-06-07T00:00:33.123+00:00'); `datetime()`
// renders space-separated 'YYYY-MM-DD HH:MM:SS', and since TEXT
// comparison is lexicographic with ' ' < 'T', mixing the two shapes
// degrades every cutoff to UTC-date granularity. Binding a chrono
// value makes both sides the same shape (sqlx encodes
// DateTime<Utc> as RFC 3339) — exact comparisons, and the injectable
// cutoff also makes boundary semantics unit-testable.

/// How often the cleanup task scans for stale rows. 5 minutes is
/// short enough that the operator-observable chip lag is bounded,
/// long enough to keep the load trivial on a SQLite-backed
/// projection.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long a `pending` row may sit before the cleanup considers
/// it expired.
const PENDING_TIMEOUT_HOURS: i64 = 1;

/// How long an in-flight `execution_results` row (`finished_at IS
/// NULL` — `events.started` landed but no `ExecResult` ever did) may
/// sit before the cleanup reaps it: stamps `finished_at`, sets a
/// sentinel `exit_code`, and annotates `stderr` so the Activity row
/// stops showing "実行中" forever.
///
/// Why this is needed: nothing else transitions these rows. The
/// results projector only flips `finished_at` when a real `ExecResult`
/// arrives; if the agent dies mid-run (or, pre-v0.43.14, hangs because
/// an orphaned grandchild kept the stdout pipe open — see PR #330),
/// no result is ever emitted and the row is stuck in-flight
/// permanently.
///
/// #682: this is now only the FALLBACK cutoff for in-flight rows whose
/// per-run `expires_at` is NULL — legacy rows written before the column
/// existed, and any run whose manifest/timeout the events projector
/// couldn't resolve. Rows that DO carry `expires_at` (the common case)
/// are reaped the moment `now > expires_at` instead, i.e. roughly at
/// `recorded_at + timeout + slack` — minutes after an abandoned short
/// run, not a day. See `projector::events::reap_deadline`.
///
/// Why 24 h for the fallback (vs the 1 h pending timeout): a legitimately
/// long run — e.g. a job that drives an interactive `claude` session —
/// can sit `finished_at IS NULL` for hours by design, so the blind
/// threshold must clear the longest plausible real run. Post-#330 every
/// run is force-killed at its `timeout_secs` and returns a result, so a
/// row still NULL after 24 h with no deadline is genuinely abandoned.
const INFLIGHT_TIMEOUT_HOURS: i64 = 24;

/// Sentinel `exit_code` stamped on a reaped in-flight row. `-1`
/// matches the agent's own convention for Killed / Timeout outcomes
/// (see kanade-agent `commands.rs`), so SPA consumers that already
/// treat negative codes as "did not exit cleanly" need no change.
const REAPED_EXIT_CODE: i64 = -1;

/// Note appended to a reaped row's `stderr` so an operator opening
/// the Activity detail sees WHY the row finished without real output.
const REAPED_STDERR_NOTE: &str = "[backend: reaped — no ExecResult before the run's deadline; agent likely \
     died mid-run or hit the pre-v0.43.14 kill-hang (#330)]";

/// v0.31 / #41: `inventory_history` retention. 90 d is enough for
/// rollout-curve / first-seen use cases without unbounded growth.
/// The change-only design already bounds row volume to actual
/// fleet churn; this just bounds the tail. Operator-tunable via
/// config in a follow-up.
const HISTORY_RETENTION_DAYS: i64 = 90;

/// v0.40 Part 1: `host_perf_samples` retention. 30 d is the SPA's
/// longest range selector and covers month-over-month investigations
/// (rare). At 60 s sample cadence × 30 d × 1000 PCs ≈ 43 M rows,
/// which SQLite handles fine. Beyond 30 d a rollup pass is more
/// appropriate than a raw retention bump — TBD when fleets grow.
const PERF_RETENTION_DAYS: i64 = 30;

/// v0.41 / Phase 2: `process_perf_samples` retention. 7 d is much
/// tighter than host_perf because process-perf is N rows per tick
/// (top-N) instead of 1, and the operator use case is "investigation
/// now / a few hours back", not "monthly trend". Process-perf only
/// populates while an operator has flipped a PC into investigation
/// mode, so the absolute row count is bounded by active windows
/// regardless of fleet size.
const PROCESS_PERF_RETENTION_DAYS: i64 = 7;

/// Issue #246: `obs_events` retention. 90 d matches the
/// `inventory_history` window so operators have one mental "how
/// far back can I look" answer across timeline surfaces. Cadence
/// is low (~50/day/PC), so 90 d × 1000 PCs ≈ 4.5 M rows — easily
/// within SQLite limits.
const OBS_EVENTS_RETENTION_DAYS: i64 = 90;

/// #486: `execution_results` retention. The hottest table in the
/// projection had NO retention at all — at fleet scale the stock
/// schedules produce O(10⁵) rows/day, each carrying up to 256 KB of
/// inline stdout/stderr, so the DB grew without bound and every
/// query touching the table (scheduler dedup, /api/results, health
/// rollups) degraded monotonically. 90 d matches the
/// inventory_history / obs_events "how far back can I look" answer
/// and comfortably exceeds STREAM_RESULTS's 30 d max_age, so the
/// projection always retains strictly more history than the stream
/// can replay (a retention-emptied table therefore implies an empty
/// stream, keeping consumer_reset's empty-table heuristic benign —
/// see #529).
const RESULTS_RETENTION_DAYS: i64 = 90;

/// #486: `executions` retention, matched to
/// [`RESULTS_RETENTION_DAYS`] so an executions row never outlives
/// the per-PC results it summarises. All statuses qualify: pending
/// flips to expired after 1 h (above), and a row still `running` 90
/// days after initiation is definitively abandoned, not slow.
const EXECUTIONS_RETENTION_DAYS: i64 = 90;

/// #486: `audit_log` retention. Generous (one year) because the
/// audit trail is the compliance record, but bounded: the scheduler
/// writes one row per dispatch, so an unbounded table eventually
/// dominates the DB and the unindexed-newest-50 listing (#516).
/// Operators needing longer horizons should archive externally —
/// STREAM_AUDIT retains the authoritative event log.
const AUDIT_RETENTION_DAYS: i64 = 365;

/// #486: retention DELETEs run in bounded batches so a large
/// backlog (first sweep after the upgrade, or after downtime) can't
/// hold SQLite's single writer lock for one giant transaction. Each
/// batch is its own implicit transaction; the per-tick cap bounds a
/// tick to `BATCH × MAX_BATCHES` rows, and any remainder simply
/// drains on subsequent 5-minute ticks.
const RETENTION_DELETE_BATCH: i64 = 10_000;
const RETENTION_MAX_BATCHES_PER_TICK: u32 = 10;

/// Spawn the long-running cleanup task. Runs forever; logs a warn
/// on transient SQLite errors and continues to the next tick. The
/// task is fire-and-forget — the returned handle is for the
/// caller to (optionally) hold so the runtime keeps the task
/// alive.
pub fn spawn(pool: SqlitePool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            interval_secs = CLEANUP_INTERVAL.as_secs(),
            pending_timeout_hours = PENDING_TIMEOUT_HOURS,
            "executions cleanup task started",
        );
        // Gemini #77 medium fix: `tokio::time::interval` keeps a
        // consistent cadence by accounting for the cleanup body's
        // execution time, vs the previous `sleep`-after-work which
        // drifts the period by however long the UPDATE took. First
        // `tick().await` fires immediately, preserving the
        // run-on-spawn behaviour.
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
        loop {
            interval.tick().await;
            // One `now` per tick: every sweep's cutoff derives from
            // the same instant, so a tick is internally consistent.
            let now = Utc::now();
            match expire_stale_pending(&pool, now - chrono::Duration::hours(PENDING_TIMEOUT_HOURS))
                .await
            {
                Ok(n) if n > 0 => info!(
                    expired = n,
                    "executions cleanup: marked {n} stale pending rows as expired",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "executions cleanup failed"),
            }
            // PR #330 follow-up: reap in-flight `execution_results`
            // rows whose `ExecResult` never arrived (agent died /
            // pre-#330 kill-hang). Without this they show "実行中"
            // forever on the Activity page. Shares the 5 min timer.
            // #682: the reaper now keys on each row's per-run
            // `expires_at` (with the 24h cutoff as the NULL fallback),
            // so it passes the tick `now` and derives both internally.
            match reap_orphaned_results(&pool, now).await {
                Ok(n) if n > 0 => info!(
                    reaped = n,
                    "execution_results cleanup: reaped {n} orphaned in-flight rows past their reap deadline",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "execution_results reap failed"),
            }
            // v0.31 / #41: prune inventory_history rows older than
            // HISTORY_RETENTION_DAYS. Same 5 min cadence as executions
            // cleanup so both tasks share the timer rather than
            // running parallel sweepers.
            match prune_inventory_history(
                &pool,
                now - chrono::Duration::days(HISTORY_RETENTION_DAYS),
            )
            .await
            {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "inventory_history cleanup: pruned {n} rows older than {HISTORY_RETENTION_DAYS}d",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "inventory_history cleanup failed"),
            }
            // v0.40 Part 1: prune host_perf_samples rows older than
            // PERF_RETENTION_DAYS. Same shared-timer pattern.
            match prune_host_perf_samples(&pool, now - chrono::Duration::days(PERF_RETENTION_DAYS))
                .await
            {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "host_perf_samples cleanup: pruned {n} rows older than {PERF_RETENTION_DAYS}d",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "host_perf_samples cleanup failed"),
            }
            // v0.41 / Phase 2: prune process_perf_samples rows older
            // than PROCESS_PERF_RETENTION_DAYS. Tighter retention than
            // host_perf because the table is N rows per tick.
            match prune_process_perf_samples(
                &pool,
                now - chrono::Duration::days(PROCESS_PERF_RETENTION_DAYS),
            )
            .await
            {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "process_perf_samples cleanup: pruned {n} rows older than {PROCESS_PERF_RETENTION_DAYS}d",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "process_perf_samples cleanup failed"),
            }
            // Issue #246: prune obs_events rows older than
            // OBS_EVENTS_RETENTION_DAYS. Same shared-timer pattern.
            match prune_obs_events(
                &pool,
                now - chrono::Duration::days(OBS_EVENTS_RETENTION_DAYS),
            )
            .await
            {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "obs_events cleanup: pruned {n} rows older than {OBS_EVENTS_RETENTION_DAYS}d",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "obs_events cleanup failed"),
            }
            // #486: prune execution_results / executions / audit_log —
            // the three previously-unbounded tables. Batched DELETEs
            // (see RETENTION_DELETE_BATCH) so the first post-upgrade
            // sweep over months of backlog can't monopolise the
            // writer lock.
            match prune_execution_results(
                &pool,
                now - chrono::Duration::days(RESULTS_RETENTION_DAYS),
            )
            .await
            {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "execution_results cleanup: pruned {n} rows older than {RESULTS_RETENTION_DAYS}d",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "execution_results retention failed"),
            }
            match prune_executions(
                &pool,
                now - chrono::Duration::days(EXECUTIONS_RETENTION_DAYS),
            )
            .await
            {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "executions cleanup: pruned {n} rows older than {EXECUTIONS_RETENTION_DAYS}d",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "executions retention failed"),
            }
            match prune_audit_log(&pool, now - chrono::Duration::days(AUDIT_RETENTION_DAYS)).await {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "audit_log cleanup: pruned {n} rows older than {AUDIT_RETENTION_DAYS}d",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "audit_log retention failed"),
            }
        }
    })
}

/// #486: delete `execution_results` rows whose `recorded_at`
/// predates the cutoff, in batches (see
/// [`RETENTION_DELETE_BATCH`]). `recorded_at` is the JetStream
/// publish time stamped by the projector — the right axis for "how
/// old is this history", and indexed by
/// `idx_execution_results_recorded_at` (added alongside this sweep)
/// so the no-op common case is an index probe, not a table scan.
async fn prune_execution_results(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    // ORDER BY makes the capped batch deterministic (oldest first)
    // and is free: it matches the index's natural scan order.
    prune_batched(
        pool,
        "DELETE FROM execution_results
          WHERE rowid IN (
              SELECT rowid FROM execution_results
               WHERE recorded_at < ?
               ORDER BY recorded_at
               LIMIT ?)",
        cutoff,
        "execution_results",
    )
    .await
}

/// #486: delete `executions` rows whose `initiated_at` predates the
/// cutoff — all statuses (see [`EXECUTIONS_RETENTION_DAYS`]).
///
/// Axis mismatch note (PR #541 review): results prune on
/// `recorded_at`, executions on `initiated_at`, so in principle a
/// parent could be deleted while results recorded inside the window
/// survive (reachable only via pc/job, not the executions join). In
/// practice the gap between initiation and the last recorded result
/// is bounded by job timeout + outbox drain — minutes-to-hours, not
/// the 90-day window — so the windows are effectively aligned; the
/// stragglers age out on their own axis shortly after.
async fn prune_executions(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    prune_batched(
        pool,
        "DELETE FROM executions
          WHERE rowid IN (
              SELECT rowid FROM executions
               WHERE initiated_at < ?
               ORDER BY initiated_at
               LIMIT ?)",
        cutoff,
        "executions",
    )
    .await
}

/// #486: delete `audit_log` rows whose `occurred_at` predates the
/// cutoff. Indexed by `idx_audit_log_occurred_at` (added alongside
/// this sweep, which also fixes the unindexed `ORDER BY occurred_at`
/// listing — #516).
async fn prune_audit_log(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    prune_batched(
        pool,
        "DELETE FROM audit_log
          WHERE rowid IN (
              SELECT rowid FROM audit_log
               WHERE occurred_at < ?
               ORDER BY occurred_at
               LIMIT ?)",
        cutoff,
        "audit_log",
    )
    .await
}

/// Run a batched retention DELETE: execute `sql` (a DELETE whose
/// subquery binds `cutoff` then [`RETENTION_DELETE_BATCH`]) until a
/// batch comes back short or the per-tick cap is hit. Each batch is
/// its own implicit transaction, so the writer lock is released
/// between batches and concurrent projector writes interleave.
async fn prune_batched(
    pool: &SqlitePool,
    sql: &'static str,
    cutoff: DateTime<Utc>,
    table: &'static str,
) -> Result<u64> {
    let mut total: u64 = 0;
    for _ in 0..RETENTION_MAX_BATCHES_PER_TICK {
        let rows = sqlx::query(sql)
            .bind(cutoff)
            .bind(RETENTION_DELETE_BATCH)
            .execute(pool)
            .await
            .with_context(|| format!("DELETE {table} retention batch"))?;
        let n = rows.rows_affected();
        total += n;
        if n < RETENTION_DELETE_BATCH as u64 {
            return Ok(total);
        }
    }
    // Cap hit — backlog remains; the next 5-minute tick continues.
    info!(
        table,
        deleted = total,
        "retention sweep hit per-tick batch cap; remainder drains next tick",
    );
    Ok(total)
}

/// Delete `obs_events` rows older than [`OBS_EVENTS_RETENTION_DAYS`].
/// `idx_obs_events_pc_at` covers `at DESC` range scans cheaply, so
/// the DELETE walks the natural index order even at a few million
/// rows.
async fn prune_obs_events(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM obs_events
          WHERE at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    .context("DELETE obs_events retention sweep")?;
    Ok(rows.rows_affected())
}

/// Delete `process_perf_samples` rows older than
/// [`PROCESS_PERF_RETENTION_DAYS`]. The `at` column is indexed
/// (`idx_process_perf_samples_at`) so the scan stays cheap.
async fn prune_process_perf_samples(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM process_perf_samples
          WHERE at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    .context("DELETE process_perf_samples retention sweep")?;
    Ok(rows.rows_affected())
}

/// Delete `host_perf_samples` rows older than [`PERF_RETENTION_DAYS`].
/// Returns the number of rows affected. The `at` column is indexed
/// (`idx_host_perf_samples_at`) so this scans efficiently even with
/// tens of millions of rows.
async fn prune_host_perf_samples(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM host_perf_samples
          WHERE at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    .context("DELETE host_perf_samples retention sweep")?;
    Ok(rows.rows_affected())
}

/// Delete `inventory_history` rows older than
/// [`HISTORY_RETENTION_DAYS`]. Returns the number of rows affected.
async fn prune_inventory_history(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM inventory_history
          WHERE observed_at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    .context("DELETE inventory_history retention sweep")?;
    Ok(rows.rows_affected())
}

/// Flip every `executions.status = 'pending'` row older than
/// the cutoff to `'expired'`. Returns the number of rows
/// affected so the caller can log a one-line summary. Idempotent —
/// rows already in `'expired'` (or any non-pending state) are
/// untouched.
///
/// Gemini #77 medium fix: the SQL string is static and the cutoff is
/// `.bind()`'d as a parameter instead of being `format!`'d into the
/// literal — parameterised queries are the SQL-idiomatic style + let
/// the driver reuse prepared statements. #390 moved the cutoff
/// computation Rust-side (chrono) so both comparison operands share
/// the RFC 3339 text shape.
async fn expire_stale_pending(pool: &SqlitePool, cutoff: DateTime<Utc>) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE executions
            SET status = 'expired'
          WHERE status = 'pending'
            AND initiated_at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    .context("UPDATE executions expire stale pending")?;
    Ok(rows.rows_affected())
}

/// Reap abandoned in-flight `execution_results` rows (`finished_at IS
/// NULL`): stamp `finished_at = now`, set [`REAPED_EXIT_CODE`], mark
/// `reaped = 1`, and append [`REAPED_STDERR_NOTE`] to `stderr`. Returns
/// the number of rows affected. Idempotent — the `finished_at IS NULL`
/// guard means a row reaped on one tick is invisible to the next.
///
/// #682: a row is "abandoned" once `now` passes its per-run
/// `expires_at` (= `recorded_at + timeout + slack`, stamped by the
/// events projector). Rows without an `expires_at` (legacy rows, or runs
/// whose manifest/timeout couldn't be resolved) fall back to the blind
/// `started_at < now - INFLIGHT_TIMEOUT_HOURS` cutoff — the pre-#682
/// behaviour. `now` is the cleanup tick's single timestamp, in the same
/// clock domain as the `recorded_at` the deadline was computed from.
///
/// `reaped = 1` tags the row as a placeholder so the results
/// projector can still overwrite it if the *real* `ExecResult`
/// arrives late (migration 0010 / gemini review on #332); on that
/// overwrite the projector clears the flag back to 0.
///
/// SQLite serves the nested OR by scanning the `expires_at` partial
/// index `idx_execution_results_expires` and applying the OR as a filter
/// (verified via EXPLAIN QUERY PLAN: `SCAN ... USING INDEX
/// idx_execution_results_expires`) — NOT a two-index UNION across both
/// partial indexes (the nested-OR form doesn't qualify for that). It
/// doesn't need to: because that index is scoped to `finished_at IS
/// NULL`, the scan visits only the small in-flight working set and never
/// touches the finished-row history, so it stays cheap regardless of how
/// large the history grows. (`idx_execution_results_inflight` on
/// `started_at` still serves the coverage/scheduler in-flight reads.)
///
/// `stderr` is appended-to rather than overwritten so any partial
/// capture the agent DID manage to ship before dying survives; the
/// `CASE` keeps the note flush against the top when `stderr` was
/// empty (the common in-flight case, since the row was created by
/// `events.started` with the default empty string).
async fn reap_orphaned_results(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64> {
    let fallback_cutoff = now - chrono::Duration::hours(INFLIGHT_TIMEOUT_HOURS);
    // #390: finished_at is stamped from a chrono bind (RFC 3339), not
    // CURRENT_TIMESTAMP — the latter's space-separated text was the
    // one writer leaking a second format into a chrono-bound column.
    let rows = sqlx::query(
        "UPDATE execution_results
            SET finished_at = ?,
                exit_code   = ?,
                reaped      = 1,
                stderr      = CASE
                    WHEN stderr = '' THEN ?
                    ELSE stderr || char(10) || ?
                END
          WHERE finished_at IS NULL
            AND (
                  (expires_at IS NOT NULL AND expires_at < ?)
               OR (expires_at IS NULL     AND started_at < ?)
            )",
    )
    .bind(now)
    .bind(REAPED_EXIT_CODE)
    .bind(REAPED_STDERR_NOTE)
    .bind(REAPED_STDERR_NOTE)
    .bind(now)
    .bind(fallback_cutoff)
    .execute(pool)
    .await
    .context("UPDATE execution_results reap orphaned in-flight")?;
    Ok(rows.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// Cutoff matching the tick loop's computation, evaluated at
    /// call time.
    fn pending_cutoff() -> DateTime<Utc> {
        Utc::now() - chrono::Duration::hours(PENDING_TIMEOUT_HOURS)
    }

    /// Insert an executions row at a chosen `initiated_at` offset
    /// from now, bound through chrono (RFC 3339) like the production
    /// writer in `api::exec`. `offset_minutes` negative = in the
    /// past. Returns the exact stamp for boundary assertions.
    async fn insert_exec(
        pool: &SqlitePool,
        exec_id: &str,
        status: &str,
        offset_minutes: i64,
    ) -> DateTime<Utc> {
        let initiated_at = Utc::now() + chrono::Duration::minutes(offset_minutes);
        sqlx::query(
            "INSERT INTO executions
                (exec_id, job_id, version, initiated_by, target_count, status, initiated_at)
             VALUES (?, 'j', '1.0', 'tester', 1, ?, ?)",
        )
        .bind(exec_id)
        .bind(status)
        .bind(initiated_at)
        .execute(pool)
        .await
        .unwrap();
        initiated_at
    }

    #[tokio::test]
    async fn pending_older_than_1h_becomes_expired() {
        let pool = fresh_pool().await;
        insert_exec(&pool, "e-stale", "pending", -120).await; // 2h ago
        let affected = expire_stale_pending(&pool, pending_cutoff()).await.unwrap();
        assert_eq!(affected, 1);
        let status: (String,) = sqlx::query_as("SELECT status FROM executions WHERE exec_id = ?")
            .bind("e-stale")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status.0, "expired");
    }

    #[tokio::test]
    async fn pending_within_1h_is_left_alone() {
        let pool = fresh_pool().await;
        insert_exec(&pool, "e-fresh", "pending", -30).await; // 30 min ago
        let affected = expire_stale_pending(&pool, pending_cutoff()).await.unwrap();
        assert_eq!(affected, 0, "fresh pending must not be touched");
        let status: (String,) = sqlx::query_as("SELECT status FROM executions WHERE exec_id = ?")
            .bind("e-fresh")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status.0, "pending");
    }

    #[tokio::test]
    async fn other_statuses_are_never_touched() {
        // running / completed / expired all stay put even if
        // older than the cutoff. Cleanup is specifically scoped
        // to pending — running rows have data and shouldn't be
        // silently demoted.
        let pool = fresh_pool().await;
        insert_exec(&pool, "e-run", "running", -180).await;
        insert_exec(&pool, "e-done", "completed", -180).await;
        insert_exec(&pool, "e-exp", "expired", -180).await;
        let affected = expire_stale_pending(&pool, pending_cutoff()).await.unwrap();
        assert_eq!(affected, 0);
        for (id, expected) in [
            ("e-run", "running"),
            ("e-done", "completed"),
            ("e-exp", "expired"),
        ] {
            let status: (String,) =
                sqlx::query_as("SELECT status FROM executions WHERE exec_id = ?")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(status.0, expected, "{id} status should be unchanged");
        }
    }

    #[tokio::test]
    async fn pending_exactly_at_cutoff_is_left_alone() {
        // CodeRabbit #77 boundary test: lock in the `<` (strict)
        // semantic on the cutoff. A row whose `initiated_at` equals
        // the cutoff exactly must NOT be expired. The injectable
        // cutoff (#390) makes the equality exact — we pass the row's
        // own stamp as the cutoff. If anyone ever swaps the
        // comparison to `<=`, this test fails loudly.
        let pool = fresh_pool().await;
        let stamp = insert_exec(&pool, "e-boundary", "pending", -60).await;
        let affected = expire_stale_pending(&pool, stamp).await.unwrap();
        assert_eq!(affected, 0, "row exactly at the cutoff is at the boundary");
        let status: (String,) = sqlx::query_as("SELECT status FROM executions WHERE exec_id = ?")
            .bind("e-boundary")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status.0, "pending");
    }

    #[tokio::test]
    async fn cleanup_is_idempotent() {
        let pool = fresh_pool().await;
        insert_exec(&pool, "e-old", "pending", -120).await;
        let first = expire_stale_pending(&pool, pending_cutoff()).await.unwrap();
        let second = expire_stale_pending(&pool, pending_cutoff()).await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "second run finds nothing to expire");
    }

    /// Insert an in-flight `execution_results` row (`finished_at
    /// IS NULL`, `exit_code IS NULL`) at a chosen `started_at`
    /// offset, bound through chrono (RFC 3339) like the production
    /// writers. `offset_minutes` negative = in the past.
    async fn insert_inflight_result(
        pool: &SqlitePool,
        result_id: &str,
        offset_minutes: i64,
        stderr: &str,
    ) {
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at)
             VALUES (?, 'req', 'pc-1', NULL, '', ?, ?, NULL)",
        )
        .bind(result_id)
        .bind(stderr)
        .bind(Utc::now() + chrono::Duration::minutes(offset_minutes))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn inflight_older_than_24h_is_reaped() {
        let pool = fresh_pool().await;
        insert_inflight_result(&pool, "r-stale", -25 * 60, "").await; // 25h ago
        let n = reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        assert_eq!(n, 1);
        let row: (Option<String>, Option<i64>, String, i64) = sqlx::query_as(
            "SELECT finished_at, exit_code, stderr, reaped \
             FROM execution_results WHERE result_id = ?",
        )
        .bind("r-stale")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.0.is_some(), "finished_at must be stamped");
        assert_eq!(row.1, Some(REAPED_EXIT_CODE), "sentinel exit_code set");
        assert!(row.2.contains("reaped"), "stderr must carry the reap note");
        assert_eq!(row.3, 1, "row must be flagged reaped = 1");
    }

    #[tokio::test]
    async fn inflight_within_24h_is_left_alone() {
        // A run that's only been in-flight an hour might be a
        // legitimately long job — must NOT be reaped.
        let pool = fresh_pool().await;
        insert_inflight_result(&pool, "r-fresh", -60, "").await; // 1h ago
        let n = reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        assert_eq!(n, 0, "fresh in-flight row must not be touched");
        let fin: (Option<String>,) =
            sqlx::query_as("SELECT finished_at FROM execution_results WHERE result_id = ?")
                .bind("r-fresh")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(fin.0.is_none(), "row stays in-flight (finished_at NULL)");
    }

    #[tokio::test]
    async fn already_finished_rows_are_untouched() {
        // A finished row older than the cutoff must never be
        // re-stamped or re-annotated — the `finished_at IS NULL`
        // guard excludes it.
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at)
             VALUES ('r-done', 'req', 'pc-1', 0, '', '', ?, ?)",
        )
        .bind(Utc::now() - chrono::Duration::hours(48))
        .bind(Utc::now() - chrono::Duration::hours(47))
        .execute(&pool)
        .await
        .unwrap();
        let n = reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        assert_eq!(n, 0);
        let row: (Option<i64>, String) =
            sqlx::query_as("SELECT exit_code, stderr FROM execution_results WHERE result_id = ?")
                .bind("r-done")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, Some(0), "finished row's exit_code unchanged");
        assert!(!row.1.contains("reaped"), "no note added to finished row");
    }

    #[tokio::test]
    async fn reap_appends_note_after_partial_stderr() {
        // Any partial capture the agent shipped before dying must
        // survive; the note is appended, not overwritten.
        let pool = fresh_pool().await;
        insert_inflight_result(&pool, "r-partial", -25 * 60, "partial output").await;
        reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        let s: (String,) =
            sqlx::query_as("SELECT stderr FROM execution_results WHERE result_id = ?")
                .bind("r-partial")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            s.0.starts_with("partial output"),
            "partial capture kept first"
        );
        assert!(
            s.0.contains("reaped"),
            "note appended after the partial bytes"
        );
    }

    #[tokio::test]
    async fn reap_is_idempotent() {
        let pool = fresh_pool().await;
        insert_inflight_result(&pool, "r-old", -30 * 60, "").await; // 30h ago
        let first = reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        let second = reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "reaped row is no longer in-flight");
    }

    /// Insert an in-flight row with an explicit per-run `expires_at`
    /// (#682), both offsets in minutes from now (negative = past).
    async fn insert_inflight_with_expiry(
        pool: &SqlitePool,
        result_id: &str,
        started_offset_min: i64,
        expires_offset_min: i64,
    ) {
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, expires_at)
             VALUES (?, 'req', 'pc-1', NULL, '', '', ?, NULL, ?)",
        )
        .bind(result_id)
        .bind(Utc::now() + chrono::Duration::minutes(started_offset_min))
        .bind(Utc::now() + chrono::Duration::minutes(expires_offset_min))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn inflight_past_expires_at_is_reaped_before_24h() {
        // #682: a short run abandoned mid-flight is reaped as soon as
        // its per-run deadline passes — minutes, not a full day. The
        // row started only 10 min ago (far inside the 24h fallback) but
        // its expires_at is already 5 min in the past.
        let pool = fresh_pool().await;
        insert_inflight_with_expiry(&pool, "r-deadline", -10, -5).await;
        let n = reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        assert_eq!(n, 1, "row past its expires_at must be reaped early");
        let row: (Option<String>, Option<i64>, i64) = sqlx::query_as(
            "SELECT finished_at, exit_code, reaped \
             FROM execution_results WHERE result_id = ?",
        )
        .bind("r-deadline")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.0.is_some(), "finished_at stamped");
        assert_eq!(row.1, Some(REAPED_EXIT_CODE));
        assert_eq!(row.2, 1);
    }

    #[tokio::test]
    async fn inflight_before_expires_at_survives_past_24h() {
        // #682: a legitimately long run keeps its full timeout. Even
        // though it started 30h ago (past the blind 24h fallback), its
        // expires_at is still in the future, so it must NOT be reaped —
        // the deadline arm wins and the NULL-only fallback never fires.
        let pool = fresh_pool().await;
        insert_inflight_with_expiry(&pool, "r-longrun", -30 * 60, 60).await;
        let n = reap_orphaned_results(&pool, Utc::now()).await.unwrap();
        assert_eq!(n, 0, "a run before its deadline must survive past 24h");
        let fin: (Option<String>,) =
            sqlx::query_as("SELECT finished_at FROM execution_results WHERE result_id = ?")
                .bind("r-longrun")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(fin.0.is_none(), "row stays in-flight");
    }

    #[tokio::test]
    async fn inflight_at_exact_expires_at_boundary_survives() {
        // Strict `<` boundary (matches the pending/finished cutoff tests):
        // a row whose expires_at equals `now` exactly must NOT be reaped.
        let pool = fresh_pool().await;
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, expires_at)
             VALUES ('r-boundary', 'req', 'pc-1', NULL, '', '', ?, NULL, ?)",
        )
        .bind(now - chrono::Duration::minutes(10))
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let n = reap_orphaned_results(&pool, now).await.unwrap();
        assert_eq!(n, 0, "row exactly at expires_at must not be reaped");
    }

    /// Insert a finished `execution_results` row with a chosen
    /// `recorded_at` offset (days, negative = past).
    async fn insert_finished_result(pool: &SqlitePool, result_id: &str, offset_days: i64) {
        let at = Utc::now() + chrono::Duration::days(offset_days);
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at)
             VALUES (?, 'req', 'pc-1', 0, '', '', ?, ?, ?)",
        )
        .bind(result_id)
        .bind(at)
        .bind(at)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
    }

    fn results_cutoff() -> DateTime<Utc> {
        Utc::now() - chrono::Duration::days(RESULTS_RETENTION_DAYS)
    }

    #[tokio::test]
    async fn old_execution_results_are_pruned_fresh_kept() {
        let pool = fresh_pool().await;
        insert_finished_result(&pool, "r-ancient", -120).await; // 120d ago
        insert_finished_result(&pool, "r-recent", -30).await; // 30d ago
        let n = prune_execution_results(&pool, results_cutoff())
            .await
            .unwrap();
        assert_eq!(n, 1, "only the >90d row is pruned");
        let left: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM execution_results")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left.0, 1);
        let id: (String,) = sqlx::query_as("SELECT result_id FROM execution_results")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(id.0, "r-recent");
    }

    #[tokio::test]
    async fn old_executions_all_statuses_are_pruned() {
        let pool = fresh_pool().await;
        for (id, status) in [("e-c", "completed"), ("e-r", "running"), ("e-x", "expired")] {
            insert_exec(&pool, id, status, -120 * 24 * 60).await; // 120d ago
        }
        insert_exec(&pool, "e-new", "completed", -24 * 60).await; // 1d ago
        let cutoff = Utc::now() - chrono::Duration::days(EXECUTIONS_RETENTION_DAYS);
        let n = prune_executions(&pool, cutoff).await.unwrap();
        assert_eq!(n, 3, "all >90d rows pruned regardless of status");
        let left: (String,) = sqlx::query_as("SELECT exec_id FROM executions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left.0, "e-new");
    }

    #[tokio::test]
    async fn old_audit_rows_are_pruned_fresh_kept() {
        let pool = fresh_pool().await;
        for (id, days) in [("old", -400i64), ("new", -10)] {
            sqlx::query(
                "INSERT INTO audit_log (actor, action, target, occurred_at)
                 VALUES ('tester', ?, NULL, ?)",
            )
            .bind(id)
            .bind(Utc::now() + chrono::Duration::days(days))
            .execute(&pool)
            .await
            .unwrap();
        }
        let cutoff = Utc::now() - chrono::Duration::days(AUDIT_RETENTION_DAYS);
        let n = prune_audit_log(&pool, cutoff).await.unwrap();
        assert_eq!(n, 1);
        let left: (String,) = sqlx::query_as("SELECT action FROM audit_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left.0, "new");
    }

    #[tokio::test]
    async fn results_row_exactly_at_cutoff_is_kept() {
        // Lock in the strict `<` boundary semantic, mirroring
        // pending_exactly_at_cutoff_is_left_alone.
        let pool = fresh_pool().await;
        let at = Utc::now() - chrono::Duration::days(RESULTS_RETENTION_DAYS);
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at)
             VALUES ('r-edge', 'req', 'pc-1', 0, '', '', ?, ?, ?)",
        )
        .bind(at)
        .bind(at)
        .bind(at)
        .execute(&pool)
        .await
        .unwrap();
        let n = prune_execution_results(&pool, at).await.unwrap();
        assert_eq!(n, 0, "row exactly at the cutoff must survive");
    }

    #[tokio::test]
    async fn executions_row_exactly_at_cutoff_is_kept() {
        // Same strict `<` boundary lock-in as
        // results_row_exactly_at_cutoff_is_kept, for prune_executions.
        let pool = fresh_pool().await;
        let stamp = insert_exec(&pool, "e-edge", "completed", 0).await;
        let n = prune_executions(&pool, stamp).await.unwrap();
        assert_eq!(n, 0, "row exactly at the cutoff must survive");
    }

    #[tokio::test]
    async fn audit_row_exactly_at_cutoff_is_kept() {
        // Same strict `<` boundary lock-in for prune_audit_log.
        let pool = fresh_pool().await;
        let at = Utc::now() - chrono::Duration::days(AUDIT_RETENTION_DAYS);
        sqlx::query(
            "INSERT INTO audit_log (actor, action, target, occurred_at)
             VALUES ('tester', 'edge', NULL, ?)",
        )
        .bind(at)
        .execute(&pool)
        .await
        .unwrap();
        let n = prune_audit_log(&pool, at).await.unwrap();
        assert_eq!(n, 0, "row exactly at the cutoff must survive");
    }

    #[tokio::test]
    async fn retention_prune_is_idempotent() {
        let pool = fresh_pool().await;
        insert_finished_result(&pool, "r-ancient", -120).await;
        let first = prune_execution_results(&pool, results_cutoff())
            .await
            .unwrap();
        let second = prune_execution_results(&pool, results_cutoff())
            .await
            .unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0);
    }
}
