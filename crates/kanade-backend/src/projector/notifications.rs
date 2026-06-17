//! Phase E — STREAM_EVENTS consumer that projects
//! `events.notifications.acked.>` payloads into the `notification_acks`
//! table. Each row is one recipient's confirmation of a notification;
//! the SPA's `GET /api/notifications/{id}/ack_status` reads them back
//! to show "who confirmed when".
//!
//! Why a dedicated consumer on the shared EVENTS stream (rather than a
//! new stream): the ack subject lives under `events.>`, so STREAM_EVENTS
//! already retains it. A narrow `filter_subject`
//! ([`EVENTS_NOTIFICATIONS_ACKED_FILTER`]) means this projector only
//! wakes for ack events, never the high-volume `events.started.*`
//! lifecycle traffic the `events` projector handles.
//!
//! Idempotency: `ON CONFLICT(notification_id, pc_id, user_sid) DO
//! NOTHING` — JetStream redelivery of the same ack event is a no-op,
//! and a user re-acking (which the agent shouldn't emit twice, but
//! might on a retry) keeps the first recorded confirmation.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::ipc::notifications::NotificationAcked;
use kanade_shared::kv::STREAM_EVENTS;
use kanade_shared::subject::EVENTS_NOTIFICATIONS_ACKED_FILTER;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

// pub(crate): consumer_reset::reset_if_wiped names this durable when
// deciding what to drop after a projection-DB wipe (#389).
pub(crate) const CONSUMER_NAME: &str = "backend_notification_acks_projector";

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
                filter_subject: EVENTS_NOTIFICATIONS_ACKED_FILTER.into(),
                ..Default::default()
            },
        )
        .await
        .context("create notification-acks consumer")?;
    info!(
        stream = STREAM_EVENTS,
        consumer = CONSUMER_NAME,
        filter = EVENTS_NOTIFICATIONS_ACKED_FILTER,
        "notification-acks projector started"
    );

    let mut messages = consumer
        .messages()
        .await
        .context("subscribe notification-acks messages")?;
    while let Some(msg) = messages.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "notification-acks consumer error");
                continue;
            }
        };
        // recorded_at = the message's JetStream publish time, so a
        // -WipeDb re-projection (#389) reproduces the original arrival
        // times instead of stamping everything "now".
        let recorded_at = super::publish_time(&msg);
        match serde_json::from_slice::<NotificationAcked>(&msg.payload) {
            Ok(a) => {
                if let Err(err) = insert_ack_row(&pool, &a, recorded_at).await {
                    warn!(
                        error = %err,
                        notification_id = %a.notification_id,
                        pc_id = %a.pc_id,
                        user_sid = %a.user_sid,
                        "notification ack insert failed — skipping ack so JetStream redelivers",
                    );
                    // Skip ack so JetStream redelivers (SQLite-busy /
                    // transient errors are recoverable).
                    continue;
                }
                debug!(
                    notification_id = %a.notification_id,
                    pc_id = %a.pc_id,
                    user_sid = %a.user_sid,
                    "projected notification ack",
                );
            }
            Err(e) => warn!(
                error = %e,
                subject = %msg.subject,
                "deserialize NotificationAcked",
            ),
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack notification-acks message");
        }
    }
    Ok(())
}

/// Insert one recipient's confirmation. `ON CONFLICT DO NOTHING` keeps
/// the first recorded ack on redelivery / duplicate emit.
async fn insert_ack_row(
    pool: &SqlitePool,
    a: &NotificationAcked,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO notification_acks (
             notification_id, pc_id, user_sid, acked_at, recorded_at, account
         ) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(notification_id, pc_id, user_sid) DO NOTHING",
    )
    .bind(&a.notification_id)
    .bind(&a.pc_id)
    .bind(&a.user_sid)
    .bind(a.acked_at)
    .bind(recorded_at)
    .bind(&a.account)
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

    fn sample(notif: &str, pc: &str, sid: &str) -> NotificationAcked {
        NotificationAcked {
            notification_id: notif.into(),
            pc_id: pc.into(),
            user_sid: sid.into(),
            acked_at: Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 5).unwrap(),
            account: Some("EXAMPLE\\taro".into()),
        }
    }

    #[tokio::test]
    async fn ack_insert_persists_row() {
        let pool = fresh_pool().await;
        insert_ack_row(&pool, &sample("n1", "pc1", "S-1-5-21-1001"), Utc::now())
            .await
            .unwrap();
        let row: (String, chrono::DateTime<chrono::Utc>, Option<String>) = sqlx::query_as(
            "SELECT user_sid, acked_at, account FROM notification_acks WHERE notification_id = ?",
        )
        .bind("n1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "S-1-5-21-1001");
        assert_eq!(row.2.as_deref(), Some("EXAMPLE\\taro"));
    }

    #[tokio::test]
    async fn ack_redelivery_is_idempotent() {
        let pool = fresh_pool().await;
        let a = sample("n1", "pc1", "S-1-5-21-1001");
        insert_ack_row(&pool, &a, Utc::now()).await.unwrap();
        insert_ack_row(&pool, &a, Utc::now()).await.unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM notification_acks WHERE notification_id = ?")
                .bind("n1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn distinct_users_on_same_pc_each_get_a_row() {
        // Fast User Switching: two users on one PC ack the same
        // notification. The user_sid in the PK keeps them apart.
        let pool = fresh_pool().await;
        insert_ack_row(&pool, &sample("n1", "pc1", "S-1-5-21-1001"), Utc::now())
            .await
            .unwrap();
        insert_ack_row(&pool, &sample("n1", "pc1", "S-1-5-21-1002"), Utc::now())
            .await
            .unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM notification_acks WHERE notification_id = ?")
                .bind("n1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 2);
    }
}
