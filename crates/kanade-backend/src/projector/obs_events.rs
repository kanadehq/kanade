//! Issue #246 — `obs.<pc_id>` → `obs_events` table projector.
//!
//! Pulls from the `STREAM_OBS_EVENTS` JetStream stream with a
//! durable consumer, deserialises each message into a
//! [`ObsEvent`], and `INSERT OR IGNORE`s into the table. The
//! UNIQUE constraint on `(pc_id, source, event_record_id)` makes
//! agent re-sends harmless — watermark drift, outbox replay, or
//! JetStream redelivery all coalesce to the original row.
//!
//! Why JetStream (durable consumer) instead of core NATS like
//! `host_perf` does: timeline events aren't tolerant of gaps the
//! way perf charts are. A backend that's offline for 5 minutes
//! shouldn't lose every logon / boot event in that window — the
//! whole point of the timeline is "what happened, when". The cost
//! is one ack RTT per event, but cadence is low (~50/day/PC) so
//! the broker workload is trivial compared to the perf streams.
//!
//! Insert failure handling mirrors the events.started projector:
//! skip ack on error so JetStream redelivers, surface the failure
//! via warn-log for operator visibility.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::kv::STREAM_OBS_EVENTS;
use kanade_shared::subject::OBS_FILTER;
use kanade_shared::wire::ObsEvent;
use sqlx::SqlitePool;
use tracing::{info, warn};

const CONSUMER_NAME: &str = "backend_obs_events_projector";

pub async fn run(js: jetstream::Context, pool: SqlitePool) -> Result<()> {
    let stream = js
        .get_stream(STREAM_OBS_EVENTS)
        .await
        .with_context(|| format!("get stream {STREAM_OBS_EVENTS}"))?;
    let consumer = stream
        .get_or_create_consumer(
            CONSUMER_NAME,
            PullConfig {
                durable_name: Some(CONSUMER_NAME.into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                filter_subject: OBS_FILTER.into(),
                ..Default::default()
            },
        )
        .await
        .context("create obs_events consumer")?;
    info!(
        stream = STREAM_OBS_EVENTS,
        consumer = CONSUMER_NAME,
        filter = OBS_FILTER,
        "obs_events projector started"
    );

    let mut messages = consumer
        .messages()
        .await
        .context("subscribe obs_events messages")?;
    while let Some(msg) = messages.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "obs_events consumer error");
                continue;
            }
        };
        match serde_json::from_slice::<ObsEvent>(&msg.payload) {
            Ok(e) => {
                if let Err(err) = insert_event(&pool, &e).await {
                    warn!(
                        error = %err,
                        pc_id = %e.pc_id,
                        kind = %e.kind,
                        source = %e.source,
                        "obs_events insert failed — skipping ack so JetStream redelivers",
                    );
                    continue;
                }
                // Trace-level: ~50/day/PC × N PCs gets noisy at info.
                tracing::trace!(
                    pc_id = %e.pc_id,
                    kind = %e.kind,
                    source = %e.source,
                    at = %e.at,
                    "projected obs_event",
                );
            }
            Err(e) => warn!(
                error = %e,
                subject = %msg.subject,
                "deserialize ObsEvent",
            ),
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack obs_events message");
        }
    }
    Ok(())
}

/// `INSERT OR IGNORE` on the UNIQUE (pc_id, source, event_record_id)
/// constraint so the agent's outbox-replay / watermark-drift /
/// JetStream-redelivery re-sends are no-ops rather than errors.
/// `payload` is stored as the JSON text (Value::to_string()) so
/// the SPA can parse it per-kind on render.
async fn insert_event(pool: &SqlitePool, e: &ObsEvent) -> Result<()> {
    let payload_json = e.payload.to_string();
    sqlx::query(
        "INSERT OR IGNORE INTO obs_events (
             pc_id, at, kind, source, event_record_id, payload
         ) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&e.pc_id)
    .bind(e.at)
    .bind(&e.kind)
    .bind(&e.source)
    .bind(&e.event_record_id)
    .bind(&payload_json)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
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

    fn logon_event(pc_id: &str, event_record_id: &str) -> ObsEvent {
        ObsEvent {
            pc_id: pc_id.into(),
            at: Utc.with_ymd_and_hms(2026, 5, 28, 10, 41, 0).unwrap(),
            kind: "logon".into(),
            source: "winlog:Security".into(),
            event_record_id: Some(event_record_id.into()),
            payload: json!({ "user": "yukimemi", "logon_type": 2 }),
        }
    }

    #[tokio::test]
    async fn insert_event_writes_one_row() {
        let pool = fresh_pool().await;
        insert_event(&pool, &logon_event("minipc", "1234567"))
            .await
            .unwrap();
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM obs_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn duplicate_same_keys_is_no_op() {
        // Same (pc_id, source, event_record_id) → UNIQUE constraint
        // collapses the second INSERT to a no-op via OR IGNORE.
        // Models JetStream redelivery / outbox replay / agent
        // watermark drift — all expected, all harmless.
        let pool = fresh_pool().await;
        let e = logon_event("minipc", "1234567");
        insert_event(&pool, &e).await.unwrap();
        insert_event(&pool, &e).await.unwrap();
        insert_event(&pool, &e).await.unwrap();
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM obs_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn different_pcs_with_same_record_id_both_persist() {
        // Two PCs happen to have the same EventRecordID (which is
        // per-host-counter, so this is realistic on a same-day
        // boot across the fleet). The UNIQUE includes pc_id, so
        // both rows persist.
        let pool = fresh_pool().await;
        insert_event(&pool, &logon_event("minipc", "1234"))
            .await
            .unwrap();
        insert_event(&pool, &logon_event("laptop", "1234"))
            .await
            .unwrap();
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM obs_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn payload_round_trips_through_text_column() {
        // `payload` is stored as JSON text; verify the round-trip
        // via SQLite is byte-stable (no JSON re-formatting between
        // serde_json::Value::to_string() and what we read back).
        let pool = fresh_pool().await;
        let e = logon_event("minipc", "rec-1");
        insert_event(&pool, &e).await.unwrap();
        let (raw,): (String,) =
            sqlx::query_as("SELECT payload FROM obs_events WHERE event_record_id = ?")
                .bind("rec-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, e.payload);
    }

    #[tokio::test]
    async fn null_event_record_id_persists() {
        // Agent-emitted milestones (agent_started, etc.) have no
        // natural EventRecordID. They should still land in the
        // table; the UNIQUE constraint with a NULL in one column
        // behaves per SQL standard: NULL ≠ NULL, so multiple NULL
        // event_record_id rows with same (pc_id, source) ARE
        // allowed (intentional — agent_started emits one per
        // boot, you'd want each to be a separate row).
        let pool = fresh_pool().await;
        let e = ObsEvent {
            pc_id: "minipc".into(),
            at: Utc.with_ymd_and_hms(2026, 5, 28, 10, 0, 0).unwrap(),
            kind: "agent_started".into(),
            source: "agent:internal".into(),
            event_record_id: None,
            payload: serde_json::Value::Null,
        };
        insert_event(&pool, &e).await.unwrap();
        let later = ObsEvent {
            at: Utc.with_ymd_and_hms(2026, 5, 28, 11, 0, 0).unwrap(),
            ..e.clone()
        };
        insert_event(&pool, &later).await.unwrap();
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM obs_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2, "NULL ≠ NULL: each agent_started is its own row");
    }
}
