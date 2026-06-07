use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::kv::STREAM_AUDIT;
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{info, warn};

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
        match serde_json::from_slice::<AuditEventRow>(&msg.payload) {
            Ok(row) => {
                if let Err(e) = insert_audit(&pool, &row).await {
                    warn!(error = %e, action = %row.action, "insert audit_log failed");
                } else {
                    info!(actor = %row.actor, action = %row.action, target = ?row.target, "projected audit");
                }
            }
            Err(e) => warn!(error = %e, subject = %msg.subject, "deserialize audit event"),
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack audit message");
        }
    }
    Ok(())
}

async fn insert_audit(pool: &SqlitePool, row: &AuditEventRow) -> Result<()> {
    let payload_str = row.payload.to_string();
    sqlx::query(
        "INSERT INTO audit_log (actor, action, target, payload, occurred_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&row.actor)
    .bind(&row.action)
    .bind(&row.target)
    .bind(&payload_str)
    .bind(row.occurred_at)
    .execute(pool)
    .await?;
    Ok(())
}
