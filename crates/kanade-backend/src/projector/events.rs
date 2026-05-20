//! v0.30 / PR α: STREAM_EVENTS consumer that projects
//! `events.started.*.*` payloads into the `running_runs` table.
//!
//! Pair with the v0.29 results projector — together they maintain
//! the per-(exec_id, pc_id) lifecycle: this consumer inserts on
//! script-spawn, the results consumer UPDATEs `finished_at` on
//! ExecResult arrival. The SPA Running tab reads from
//! `running_runs WHERE finished_at IS NULL`.
//!
//! Redelivery handling: `INSERT ... ON CONFLICT(exec_id, pc_id) DO
//! NOTHING`. JetStream may redeliver the same event on ack timeout;
//! treating the redelivery as a no-op is correct — the existing row
//! already captures the same started_at.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::kv::STREAM_EVENTS;
use kanade_shared::subject::EVENTS_STARTED_FILTER;
use kanade_shared::wire::EventStarted;
use sqlx::SqlitePool;
use tracing::{info, warn};

const CONSUMER_NAME: &str = "backend_events_projector";

/// Spawn the consumer. Lives for the backend's lifetime. Filter is
/// scoped to `events.started.>` so future event types
/// (events.finished, events.heartbeat, etc.) need their own
/// projector — keeps each consumer's responsibility narrow.
pub async fn run(js: jetstream::Context, pool: SqlitePool) -> Result<()> {
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
        match serde_json::from_slice::<EventStarted>(&msg.payload) {
            Ok(e) => {
                if let Err(err) = insert_running(&pool, &e).await {
                    warn!(
                        error = %err,
                        exec_id = %e.exec_id,
                        pc_id = %e.pc_id,
                        "insert running_run failed — skipping ack so JetStream redelivers",
                    );
                    // Gemini #72 high fix: don't ack on insert
                    // failure. SQLite busy / transient errors get
                    // a redelivery instead of a silent loss. If
                    // the error is permanent (schema mismatch
                    // etc.) the operator sees repeated warn-logs
                    // — louder than silence.
                    continue;
                }
                info!(
                    exec_id = %e.exec_id,
                    pc_id = %e.pc_id,
                    manifest_id = %e.manifest_id,
                    "projected events.started",
                );
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

/// Idempotent INSERT with two race-safety guards:
///
///   1. `ON CONFLICT(exec_id, pc_id) DO NOTHING` — JetStream
///      redelivery (ack timeout etc.) of the SAME started event
///      hits the composite PK and is silently dropped.
///   2. `WHERE NOT EXISTS (SELECT 1 FROM execution_results
///      WHERE exec_id = ? AND pc_id = ?)` — prevents the ghost-row
///      pattern when delivery is out-of-order. Concretely: started
///      ack times out, JetStream redelivers it AFTER the matching
///      ExecResult has already landed. Without the guard, the
///      redelivered start would create a fresh row with `finished_at
///      = NULL`, leaving a "running" ghost in the Activity Running
///      tab for an exec that's actually long since finished. The
///      results projector populates `execution_results` BEFORE
///      calling `mark_run_finished`, so its presence is a reliable
///      "this run is already over" signal.
async fn insert_running(pool: &SqlitePool, e: &EventStarted) -> Result<()> {
    sqlx::query(
        "INSERT INTO running_runs (
             exec_id, pc_id, started_at, manifest_id, version
         )
         SELECT ?, ?, ?, ?, ?
          WHERE NOT EXISTS (
             SELECT 1 FROM execution_results
              WHERE exec_id = ? AND pc_id = ?
          )
         ON CONFLICT(exec_id, pc_id) DO NOTHING",
    )
    .bind(&e.exec_id)
    .bind(&e.pc_id)
    .bind(e.started_at)
    .bind(&e.manifest_id)
    .bind(&e.version)
    .bind(&e.exec_id)
    .bind(&e.pc_id)
    .execute(pool)
    .await?;
    Ok(())
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

    fn sample(exec_id: &str, pc_id: &str) -> EventStarted {
        EventStarted {
            exec_id: exec_id.into(),
            pc_id: pc_id.into(),
            started_at: Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap(),
            manifest_id: "inv-hw".into(),
            version: "1.0.0".into(),
        }
    }

    #[tokio::test]
    async fn insert_running_persists_row() {
        let pool = fresh_pool().await;
        insert_running(&pool, &sample("e1", "pc1")).await.unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM running_runs WHERE exec_id = ? AND pc_id = ?")
                .bind("e1")
                .bind("pc1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn insert_running_is_idempotent_on_conflict() {
        // JetStream redelivery of the same started event must not
        // create a second row. The composite PK (exec_id, pc_id)
        // catches the conflict and ON CONFLICT DO NOTHING leaves
        // the original row intact.
        let pool = fresh_pool().await;
        let e = sample("e1", "pc1");
        insert_running(&pool, &e).await.unwrap();
        insert_running(&pool, &e).await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM running_runs WHERE exec_id = ?")
            .bind("e1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1, "redelivery must not duplicate the row");
    }

    #[tokio::test]
    async fn out_of_order_redelivery_does_not_create_ghost_row() {
        // Real-world scenario: started ack timed out at JetStream,
        // redelivered AFTER the matching ExecResult has already
        // landed. Without the NOT EXISTS guard, the redelivered
        // start would create a fresh row with finished_at = NULL,
        // leaving the Activity Running tab showing a "running"
        // entry for an exec that's actually finished. With the
        // guard the INSERT silently no-ops because
        // execution_results already has a row for (exec, pc).
        let pool = fresh_pool().await;
        // Stage the post-finish state: execution_results has the
        // row for (e1, pc1), so we're saying "this exec is done".
        sqlx::query(
            "INSERT INTO execution_results (
                 result_id, request_id, exec_id, pc_id, exit_code,
                 stdout, stderr, started_at, finished_at
             ) VALUES (
                 'res-1', 'req-1', 'e1', 'pc1', 0,
                 '', '', '2026-05-20T12:00:00Z',
                 '2026-05-20T12:00:05Z'
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Out-of-order: events.started for the same (e1, pc1)
        // arrives later via redelivery.
        insert_running(&pool, &sample("e1", "pc1")).await.unwrap();
        // Ghost prevention: no row in running_runs.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM running_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count.0, 0,
            "started arriving after the matching result must not create a ghost in-flight row",
        );
    }

    #[tokio::test]
    async fn running_runs_supports_broadcast_fan_out_distinct_pcs() {
        // Same exec_id, different pc_ids = a broadcast Command fan
        // -out. Each PC gets its own running_runs row, which is
        // exactly the per-PC granularity the Activity Running tab
        // needs.
        let pool = fresh_pool().await;
        insert_running(&pool, &sample("e1", "pc1")).await.unwrap();
        insert_running(&pool, &sample("e1", "pc2")).await.unwrap();
        insert_running(&pool, &sample("e1", "pc3")).await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM running_runs WHERE exec_id = ?")
            .bind("e1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 3);
    }
}
