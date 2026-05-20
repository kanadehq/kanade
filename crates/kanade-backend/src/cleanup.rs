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

/// v0.31 / #41: `inventory_history` retention. 90 d is enough for
/// rollout-curve / first-seen use cases without unbounded growth.
/// The change-only design already bounds row volume to actual
/// fleet churn; this just bounds the tail. Operator-tunable via
/// config in a follow-up.
const HISTORY_RETENTION: &str = "-90 days";

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
        }
    })
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
        sqlx::query(&sql)
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
}
