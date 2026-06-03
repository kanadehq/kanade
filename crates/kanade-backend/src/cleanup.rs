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
use sqlx::SqlitePool;
use tracing::{info, warn};

/// How often the cleanup task scans for stale rows. 5 minutes is
/// short enough that the operator-observable chip lag is bounded,
/// long enough to keep the load trivial on a SQLite-backed
/// projection.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long a `pending` row may sit before the cleanup considers
/// it expired. SQLite-side as a relative-time string passed
/// directly to `datetime('now', '-1 hour')`.
const PENDING_TIMEOUT: &str = "-1 hour";

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
/// Why 24 h (vs the 1 h pending timeout): a legitimately long run —
/// e.g. a job that drives an interactive `claude` session — can sit
/// `finished_at IS NULL` for hours by design, so the threshold must
/// clear the longest plausible real run. Post-#330 every run is
/// force-killed at its `timeout_secs` and returns a result, so a row
/// still NULL after 24 h is genuinely abandoned, not slow. Tunable
/// here; deliberately generous to avoid demoting a live run.
const INFLIGHT_TIMEOUT: &str = "-24 hours";

/// Sentinel `exit_code` stamped on a reaped in-flight row. `-1`
/// matches the agent's own convention for Killed / Timeout outcomes
/// (see kanade-agent `commands.rs`), so SPA consumers that already
/// treat negative codes as "did not exit cleanly" need no change.
const REAPED_EXIT_CODE: i64 = -1;

/// Note appended to a reaped row's `stderr` so an operator opening
/// the Activity detail sees WHY the row finished without real output.
const REAPED_STDERR_NOTE: &str = "[backend: reaped — no ExecResult within 24h; agent likely died mid-run \
     or hit the pre-v0.43.14 kill-hang (#330)]";

/// v0.31 / #41: `inventory_history` retention. 90 d is enough for
/// rollout-curve / first-seen use cases without unbounded growth.
/// The change-only design already bounds row volume to actual
/// fleet churn; this just bounds the tail. Operator-tunable via
/// config in a follow-up.
const HISTORY_RETENTION: &str = "-90 days";

/// v0.40 Part 1: `host_perf_samples` retention. 30 d is the SPA's
/// longest range selector and covers month-over-month investigations
/// (rare). At 60 s sample cadence × 30 d × 1000 PCs ≈ 43 M rows,
/// which SQLite handles fine. Beyond 30 d a rollup pass is more
/// appropriate than a raw retention bump — TBD when fleets grow.
const PERF_RETENTION: &str = "-30 days";

/// v0.41 / Phase 2: `process_perf_samples` retention. 7 d is much
/// tighter than host_perf because process-perf is N rows per tick
/// (top-N) instead of 1, and the operator use case is "investigation
/// now / a few hours back", not "monthly trend". Process-perf only
/// populates while an operator has flipped a PC into investigation
/// mode, so the absolute row count is bounded by active windows
/// regardless of fleet size.
const PROCESS_PERF_RETENTION: &str = "-7 days";

/// Issue #246: `obs_events` retention. 90 d matches the
/// `inventory_history` window so operators have one mental "how
/// far back can I look" answer across timeline surfaces. Cadence
/// is low (~50/day/PC), so 90 d × 1000 PCs ≈ 4.5 M rows — easily
/// within SQLite limits.
const OBS_EVENTS_RETENTION: &str = "-90 days";

/// Spawn the long-running cleanup task. Runs forever; logs a warn
/// on transient SQLite errors and continues to the next tick. The
/// task is fire-and-forget — the returned handle is for the
/// caller to (optionally) hold so the runtime keeps the task
/// alive.
pub fn spawn(pool: SqlitePool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            interval_secs = CLEANUP_INTERVAL.as_secs(),
            pending_timeout = PENDING_TIMEOUT,
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
            match expire_stale_pending(&pool).await {
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
            match reap_orphaned_results(&pool).await {
                Ok(n) if n > 0 => info!(
                    reaped = n,
                    "execution_results cleanup: reaped {n} orphaned in-flight rows (no result within {INFLIGHT_TIMEOUT})",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "execution_results reap failed"),
            }
            // v0.31 / #41: prune inventory_history rows older than
            // HISTORY_RETENTION. Same 5 min cadence as executions
            // cleanup so both tasks share the timer rather than
            // running parallel sweepers.
            match prune_inventory_history(&pool).await {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "inventory_history cleanup: pruned {n} rows older than {HISTORY_RETENTION}",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "inventory_history cleanup failed"),
            }
            // v0.40 Part 1: prune host_perf_samples rows older than
            // PERF_RETENTION. Same shared-timer pattern.
            match prune_host_perf_samples(&pool).await {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "host_perf_samples cleanup: pruned {n} rows older than {PERF_RETENTION}",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "host_perf_samples cleanup failed"),
            }
            // v0.41 / Phase 2: prune process_perf_samples rows older
            // than PROCESS_PERF_RETENTION. Tighter retention than
            // host_perf because the table is N rows per tick.
            match prune_process_perf_samples(&pool).await {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "process_perf_samples cleanup: pruned {n} rows older than {PROCESS_PERF_RETENTION}",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "process_perf_samples cleanup failed"),
            }
            // Issue #246: prune obs_events rows older than
            // OBS_EVENTS_RETENTION. Same shared-timer pattern.
            match prune_obs_events(&pool).await {
                Ok(n) if n > 0 => info!(
                    deleted = n,
                    "obs_events cleanup: pruned {n} rows older than {OBS_EVENTS_RETENTION}",
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "obs_events cleanup failed"),
            }
        }
    })
}

/// Delete `obs_events` rows older than [`OBS_EVENTS_RETENTION`].
/// `idx_obs_events_pc_at` covers `at DESC` range scans cheaply, so
/// the DELETE walks the natural index order even at a few million
/// rows.
async fn prune_obs_events(pool: &SqlitePool) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM obs_events
          WHERE at < datetime('now', ?)",
    )
    .bind(OBS_EVENTS_RETENTION)
    .execute(pool)
    .await
    .context("DELETE obs_events retention sweep")?;
    Ok(rows.rows_affected())
}

/// Delete `process_perf_samples` rows older than
/// [`PROCESS_PERF_RETENTION`]. The `at` column is indexed
/// (`idx_process_perf_samples_at`) so the scan stays cheap.
async fn prune_process_perf_samples(pool: &SqlitePool) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM process_perf_samples
          WHERE at < datetime('now', ?)",
    )
    .bind(PROCESS_PERF_RETENTION)
    .execute(pool)
    .await
    .context("DELETE process_perf_samples retention sweep")?;
    Ok(rows.rows_affected())
}

/// Delete `host_perf_samples` rows older than [`PERF_RETENTION`].
/// Returns the number of rows affected. The `at` column is indexed
/// (`idx_host_perf_samples_at`) so this scans efficiently even with
/// tens of millions of rows.
async fn prune_host_perf_samples(pool: &SqlitePool) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM host_perf_samples
          WHERE at < datetime('now', ?)",
    )
    .bind(PERF_RETENTION)
    .execute(pool)
    .await
    .context("DELETE host_perf_samples retention sweep")?;
    Ok(rows.rows_affected())
}

/// Delete `inventory_history` rows older than [`HISTORY_RETENTION`].
/// Returns the number of rows affected.
async fn prune_inventory_history(pool: &SqlitePool) -> Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM inventory_history
          WHERE observed_at < datetime('now', ?)",
    )
    .bind(HISTORY_RETENTION)
    .execute(pool)
    .await
    .context("DELETE inventory_history retention sweep")?;
    Ok(rows.rows_affected())
}

/// Flip every `executions.status = 'pending'` row older than
/// `PENDING_TIMEOUT` to `'expired'`. Returns the number of rows
/// affected so the caller can log a one-line summary. Idempotent —
/// rows already in `'expired'` (or any non-pending state) are
/// untouched.
///
/// Gemini #77 medium fix: the SQL string is now static; the
/// `PENDING_TIMEOUT` constant is `.bind()`'d as a parameter instead
/// of being `format!`'d into the literal. `PENDING_TIMEOUT` is a
/// compile-time constant so injection risk is zero either way, but
/// parameterised queries are the SQL-idiomatic style + let the
/// driver reuse prepared statements.
async fn expire_stale_pending(pool: &SqlitePool) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE executions
            SET status = 'expired'
          WHERE status = 'pending'
            AND initiated_at < datetime('now', ?)",
    )
    .bind(PENDING_TIMEOUT)
    .execute(pool)
    .await
    .context("UPDATE executions expire stale pending")?;
    Ok(rows.rows_affected())
}

/// Reap in-flight `execution_results` rows (`finished_at IS NULL`)
/// whose `started_at` predates [`INFLIGHT_TIMEOUT`]: stamp
/// `finished_at = now`, set [`REAPED_EXIT_CODE`], mark `reaped = 1`,
/// and append [`REAPED_STDERR_NOTE`] to `stderr`. Returns the number
/// of rows affected. Idempotent — the `finished_at IS NULL` guard
/// means a row reaped on one tick is invisible to the next.
///
/// `reaped = 1` tags the row as a placeholder so the results
/// projector can still overwrite it if the *real* `ExecResult`
/// arrives late (migration 0010 / gemini review on #332); on that
/// overwrite the projector clears the flag back to 0.
///
/// `WHERE finished_at IS NULL AND started_at < ...` is served by the
/// partial index `idx_execution_results_inflight` (started_at DESC,
/// scoped to in-flight rows), so the scan stays cheap regardless of
/// how large the finished-row history grows.
///
/// `stderr` is appended-to rather than overwritten so any partial
/// capture the agent DID manage to ship before dying survives; the
/// `CASE` keeps the note flush against the top when `stderr` was
/// empty (the common in-flight case, since the row was created by
/// `events.started` with the default empty string).
async fn reap_orphaned_results(pool: &SqlitePool) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE execution_results
            SET finished_at = CURRENT_TIMESTAMP,
                exit_code   = ?,
                reaped      = 1,
                stderr      = CASE
                    WHEN stderr = '' THEN ?
                    ELSE stderr || char(10) || ?
                END
          WHERE finished_at IS NULL
            AND started_at < datetime('now', ?)",
    )
    .bind(REAPED_EXIT_CODE)
    .bind(REAPED_STDERR_NOTE)
    .bind(REAPED_STDERR_NOTE)
    .bind(INFLIGHT_TIMEOUT)
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

    /// Insert an executions row at a chosen `initiated_at` offset
    /// from now. `offset_minutes` negative = in the past.
    async fn insert_exec(pool: &SqlitePool, exec_id: &str, status: &str, offset_minutes: i64) {
        let sql = format!(
            "INSERT INTO executions
                (exec_id, job_id, version, initiated_by, target_count, status, initiated_at)
             VALUES (?, 'j', '1.0', 'tester', 1, ?, datetime('now', '{offset_minutes} minutes'))"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(exec_id)
            .bind(status)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pending_older_than_1h_becomes_expired() {
        let pool = fresh_pool().await;
        insert_exec(&pool, "e-stale", "pending", -120).await; // 2h ago
        let affected = expire_stale_pending(&pool).await.unwrap();
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
        let affected = expire_stale_pending(&pool).await.unwrap();
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
        let affected = expire_stale_pending(&pool).await.unwrap();
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
    async fn pending_exactly_1h_is_left_alone() {
        // CodeRabbit #77 boundary test: lock in the `<` (strict)
        // semantic on the cutoff. A row at exactly the timeout
        // boundary must NOT be expired — the SQL uses
        // `initiated_at < datetime('now', '-1 hour')`, so a row
        // inserted exactly -60 min ago has `initiated_at ==
        // (now - 1h)` and the strict inequality leaves it pending.
        // If anyone ever swaps the comparison to `<=`, this test
        // fails loudly.
        let pool = fresh_pool().await;
        insert_exec(&pool, "e-boundary", "pending", -60).await;
        let affected = expire_stale_pending(&pool).await.unwrap();
        assert_eq!(affected, 0, "exactly-1h-old pending is at the boundary");
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
        let first = expire_stale_pending(&pool).await.unwrap();
        let second = expire_stale_pending(&pool).await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "second run finds nothing to expire");
    }

    /// Insert an in-flight `execution_results` row (`finished_at
    /// IS NULL`, `exit_code IS NULL`) at a chosen `started_at`
    /// offset. `offset_minutes` negative = in the past.
    async fn insert_inflight_result(
        pool: &SqlitePool,
        result_id: &str,
        offset_minutes: i64,
        stderr: &str,
    ) {
        let sql = format!(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at)
             VALUES (?, 'req', 'pc-1', NULL, '', ?,
                     datetime('now', '{offset_minutes} minutes'), NULL)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(result_id)
            .bind(stderr)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn inflight_older_than_24h_is_reaped() {
        let pool = fresh_pool().await;
        insert_inflight_result(&pool, "r-stale", -25 * 60, "").await; // 25h ago
        let n = reap_orphaned_results(&pool).await.unwrap();
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
        let n = reap_orphaned_results(&pool).await.unwrap();
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
             VALUES ('r-done', 'req', 'pc-1', 0, '', '',
                     datetime('now', '-48 hours'), datetime('now', '-47 hours'))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let n = reap_orphaned_results(&pool).await.unwrap();
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
        reap_orphaned_results(&pool).await.unwrap();
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
        let first = reap_orphaned_results(&pool).await.unwrap();
        let second = reap_orphaned_results(&pool).await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "reaped row is no longer in-flight");
    }
}
