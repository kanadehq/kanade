//! #389: startup guard for the "wiped projection DB + surviving
//! durable consumer" mismatch.
//!
//! Every JetStream projector in this crate binds a *durable* consumer
//! (`backend_*_projector`), and a durable's delivery position lives on
//! the broker — not in the SQLite projection DB. Deleting `backend.db`
//! (deploy `-WipeDb` across a squashed-migration baseline, manual
//! recovery, disk replacement) therefore does NOT rewind anything: on
//! restart `get_or_create_consumer` finds the old durable parked at
//! the end of the stream and the projector resumes from there, so the
//! freshly-created tables never re-derive history even though the
//! streams still hold up to `max_age` worth of messages. That broke
//! the `-WipeDb` docstring's "most of the projector re-derives from
//! the JetStream streams" promise — observed live on 2026-06-05: the
//! RESULTS stream held 17 days of messages, the rebuilt
//! `execution_results` got only the unacked tail (~5 minutes).
//!
//! The fix: before the projectors spawn, check each projector's
//! projection table. An EMPTY table means the DB never saw a single
//! message through that projector — i.e. it is fresh or wiped — so any
//! existing durable consumer for it is stale state from a previous DB
//! life. Drop it; the projector's normal `get_or_create_consumer` then
//! recreates it with the default `DeliverPolicy::All` and replays the
//! whole stream. Replay is safe by design: results/events upsert via
//! `ON CONFLICT`, obs_events uses `INSERT OR IGNORE`, and `audit_log`
//! is empty (it only ever grows through its projector).
//!
//! Non-wipe paths are no-ops: a healthy upgrade has rows in every
//! table (skip), and a genuinely fresh install has no durable to
//! delete (`CONSUMER_NOT_FOUND`, ignored). The check is sequential and
//! runs before any projector task starts, so there is no race against
//! a projector inserting the first row mid-check.

use anyhow::Result;
use async_nats::jetstream;
use async_nats::jetstream::ErrorCode;
use async_nats::jetstream::stream::ConsumerErrorKind;
use kanade_shared::kv::{STREAM_AUDIT, STREAM_EVENTS, STREAM_OBS_EVENTS, STREAM_RESULTS};
use sqlx::SqlitePool;
use tracing::{info, warn};

/// (stream, durable consumer, projection table) for every durable
/// JetStream projector. heartbeat / host_perf / process_perf ride
/// core NATS (no durable position) and need no reset.
///
/// events shares `execution_results` with results: `events.started`
/// rows and `ExecResult` rows land in the same table, so emptiness of
/// that one table is the freshness signal for both consumers.
fn projector_consumers() -> [(&'static str, &'static str, &'static str); 4] {
    [
        (
            STREAM_RESULTS,
            super::results::CONSUMER_NAME,
            "execution_results",
        ),
        (
            STREAM_EVENTS,
            super::events::CONSUMER_NAME,
            "execution_results",
        ),
        (STREAM_AUDIT, super::audit::CONSUMER_NAME, "audit_log"),
        (
            STREAM_OBS_EVENTS,
            super::obs_events::CONSUMER_NAME,
            "obs_events",
        ),
    ]
}

/// Drop the durable consumer of every projector whose projection
/// table is empty, so the projector re-derives the table from its
/// stream. Per-consumer failures are logged and skipped — a broker
/// hiccup here must not block startup (the affected projector just
/// resumes from its old position, i.e. today's behaviour).
pub async fn reset_if_wiped(js: &jetstream::Context, pool: &SqlitePool) -> Result<()> {
    for (stream_name, consumer, table) in projector_consumers() {
        match table_is_empty(pool, table).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                warn!(error = %e, table, "consumer reset: emptiness check failed; skipping");
                continue;
            }
        }
        let stream = match js.get_stream(stream_name).await {
            Ok(s) => s,
            Err(e) => {
                // ensure_jetstream_resources ran just before us, so a
                // missing stream is a real (transient) broker problem.
                warn!(error = %e, stream = stream_name, "consumer reset: get_stream failed; skipping");
                continue;
            }
        };
        match stream.delete_consumer(consumer).await {
            Ok(_) => info!(
                stream = stream_name,
                consumer,
                table,
                "projection table is empty — dropped stale durable consumer; \
                 projector will re-derive from the stream",
            ),
            // Fresh install: no durable yet. Expected, not an error.
            Err(e)
                if matches!(
                    e.kind(),
                    ConsumerErrorKind::JetStream(js_err)
                        if js_err.error_code() == ErrorCode::CONSUMER_NOT_FOUND
                ) => {}
            Err(e) => {
                warn!(error = %e, stream = stream_name, consumer, "consumer reset: delete failed; projector resumes from old position");
            }
        }
    }
    Ok(())
}

async fn table_is_empty(pool: &SqlitePool, table: &str) -> Result<bool> {
    // `table` comes from the compile-time list above, never from
    // user input, so interpolating the identifier is safe (SQLite
    // can't bind identifiers).
    let exists: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS(SELECT 1 FROM {table})"
    )))
    .fetch_one(pool)
    .await?;
    Ok(exists.0 == 0)
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

    /// Every projection table named in `projector_consumers()` must
    /// exist in the schema — a rename there without updating the list
    /// would silently disable the wipe detection for that projector.
    #[tokio::test]
    async fn listed_tables_exist_and_start_empty() {
        let pool = fresh_pool().await;
        for (_, _, table) in projector_consumers() {
            assert!(
                table_is_empty(&pool, table).await.unwrap(),
                "{table} should exist and be empty on a fresh DB",
            );
        }
    }

    #[tokio::test]
    async fn non_empty_table_is_detected() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO execution_results
                 (result_id, request_id, pc_id, stdout, stderr, started_at)
             VALUES ('r1', 'req1', 'pc1', '', '', '2026-06-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(!table_is_empty(&pool, "execution_results").await.unwrap());
        assert!(table_is_empty(&pool, "audit_log").await.unwrap());
    }
}
