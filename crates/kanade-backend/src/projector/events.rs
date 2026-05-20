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
use futures::StreamExt;
use kanade_shared::kv::STREAM_EVENTS;
use kanade_shared::subject::EVENTS_STARTED_FILTER;
use kanade_shared::wire::EventStarted;
use sqlx::SqlitePool;
use tracing::{info, warn};

const CONSUMER_NAME: &str = "backend_events_projector";

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
                if let Err(err) = insert_inflight_row(&pool, &e).await {
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
                info!(
                    result_id = %e.result_id,
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

/// Create an in-flight row in `execution_results` (finished_at,
/// exit_code, stdout, stderr all default). `ON CONFLICT(result_id)
/// DO NOTHING` handles both same-direction redelivery (same started
/// event arriving twice) and out-of-order (ExecResult already
/// landed and inserted/updated the row, started's redelivery now
/// no-ops). Either way: one row, no ghost.
async fn insert_inflight_row(pool: &SqlitePool, e: &EventStarted) -> Result<()> {
    // `execution_results.job_id` (added in migration 0002) holds
    // the manifest id (= cmd id), NOT exec_id — naming legacy from
    // pre-v0.29 when Command.job_id was misnamed and conflated with
    // the deploy UUID. We bind EventStarted.manifest_id to it so
    // the dedup index (job_id, pc_id, finished_at DESC) keeps
    // working unchanged.
    sqlx::query(
        "INSERT INTO execution_results (
             result_id, request_id, exec_id, pc_id, started_at,
             version, job_id
         ) VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(result_id) DO NOTHING",
    )
    .bind(&e.result_id)
    .bind(&e.request_id)
    .bind(&e.exec_id)
    .bind(&e.pc_id)
    .bind(e.started_at)
    .bind(&e.version)
    .bind(&e.manifest_id)
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
        insert_inflight_row(&pool, &sample("r1", "e1", "pc1"))
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
        insert_inflight_row(&pool, &e).await.unwrap();
        insert_inflight_row(&pool, &e).await.unwrap();
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
        insert_inflight_row(&pool, &sample("r1", "e1", "pc1"))
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
        insert_inflight_row(&pool, &sample("r1", "e1", "pc1"))
            .await
            .unwrap();
        insert_inflight_row(&pool, &sample("r2", "e1", "pc2"))
            .await
            .unwrap();
        insert_inflight_row(&pool, &sample("r3", "e1", "pc3"))
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
}
