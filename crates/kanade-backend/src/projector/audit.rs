use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::kv::STREAM_AUDIT;
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

// pub(crate): consumer_reset::reset_if_wiped names this durable when
// deciding what to drop after a projection-DB wipe (#389).
pub(crate) const CONSUMER_NAME: &str = "backend_audit_projector";

/// Wire-format that mirrors `audit::AuditEvent` from the publish side.
#[derive(Deserialize, Debug)]
struct AuditEventRow {
    actor: String,
    action: String,
    target: Option<String>,
    payload: serde_json::Value,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

pub async fn run(js: jetstream::Context, pool: SqlitePool) -> Result<()> {
    let stream = js
        .get_stream(STREAM_AUDIT)
        .await
        .with_context(|| format!("get stream {STREAM_AUDIT}"))?;
    let consumer = stream
        .get_or_create_consumer(
            CONSUMER_NAME,
            PullConfig {
                durable_name: Some(CONSUMER_NAME.into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .context("create audit consumer")?;
    info!(
        stream = STREAM_AUDIT,
        consumer = CONSUMER_NAME,
        "audit projector started"
    );

    let mut messages = consumer
        .messages()
        .await
        .context("subscribe audit messages")?;
    while let Some(msg) = messages.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "audit consumer error");
                continue;
            }
        };
        // #501: the message's stream sequence is the idempotency key
        // — a redelivery carries the same sequence and INSERT OR
        // IGNORE no-ops on the partial UNIQUE index, so the
        // permanent record can no longer grow duplicates.
        let stream_seq = match msg.info() {
            Ok(info) => Some(info.stream_sequence as i64),
            Err(e) => {
                // NULL escapes the partial UNIQUE index, so this
                // message has no dedup key — make the gap auditable
                // instead of silent (PR #569 review, claude).
                warn!(
                    error = %e,
                    "audit: msg.info() failed — idempotency key unavailable, a redelivery of this message could duplicate",
                );
                None
            }
        };
        match serde_json::from_slice::<AuditEventRow>(&msg.payload) {
            Ok(row) => {
                if let Err(e) = insert_audit(&pool, &row, stream_seq).await {
                    warn!(
                        error = %e,
                        action = %row.action,
                        "insert audit_log failed — skipping ack so JetStream redelivers",
                    );
                    // #501: skip the ack (the events/obs/results
                    // discipline) — an acked-but-failed insert lost
                    // the audit row permanently, and consumer_reset
                    // won't replay a non-empty table.
                    continue;
                }
                debug!(actor = %row.actor, action = %row.action, target = ?row.target, "projected audit");
            }
            Err(e) => warn!(error = %e, subject = %msg.subject, "deserialize audit event"),
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack audit message");
        }
    }
    Ok(())
}

async fn insert_audit(
    pool: &SqlitePool,
    row: &AuditEventRow,
    stream_seq: Option<i64>,
) -> Result<()> {
    let payload_str = row.payload.to_string();
    // OR IGNORE: a redelivered message (same stream_seq) no-ops on
    // the partial UNIQUE index instead of duplicating the row (#501).
    sqlx::query(
        "INSERT OR IGNORE INTO audit_log
             (actor, action, target, payload, occurred_at, stream_seq)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.actor)
    .bind(&row.action)
    .bind(&row.target)
    .bind(&payload_str)
    .bind(row.occurred_at)
    .bind(stream_seq)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn sample_row() -> AuditEventRow {
        AuditEventRow {
            actor: "tester".into(),
            action: "exec".into(),
            target: Some("job-1".into()),
            payload: serde_json::json!({ "k": "v" }),
            occurred_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn same_stream_seq_inserts_once() {
        // #501: a JetStream redelivery carries the same stream
        // sequence; the partial UNIQUE index + INSERT OR IGNORE must
        // dedup it.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        insert_audit(&pool, &sample_row(), Some(42)).await.unwrap();
        insert_audit(&pool, &sample_row(), Some(42)).await.unwrap();
        // Pre-#501 rows have no sequence; NULLs never collide.
        insert_audit(&pool, &sample_row(), None).await.unwrap();
        insert_audit(&pool, &sample_row(), None).await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 3, "seq-42 deduped to one row; NULLs both kept");
    }
}
