//! Phase E — STREAM_EVENTS consumer that projects the notification
//! confirmation lifecycle (`events.notifications.acked.>` **and**
//! `events.notifications.unacked.>`) into two tables:
//!
//! * `notification_acks` — the **current-state read model**. One row per
//!   `(notification_id, pc_id, user_sid)`, tri-state: `acked_at` set +
//!   `unacked_at` NULL ⇒ 確認済み; both set ⇒ 取消済み (confirmed then
//!   retracted); no row ⇒ 未確認. The SPA roster aggregates this — a plain
//!   equality scan that stays O(1)-shaped no matter the fleet size.
//! * `notification_ack_events` — the **append-only audit log**. Every ack
//!   and unack, in order, so an operator can prove a user *had* confirmed
//!   before retracting ("I never saw it" is contradicted here).
//!
//! ## Why one consumer for both subjects (ordering is the whole point)
//!
//! ack (INSERT/UPSERT) and unack (UPDATE) for the *same* recipient must
//! be serialised, or a stale DELETE/UPDATE could overtake a newer INSERT
//! and corrupt the read model. A single durable consumer over the
//! broadened [`EVENTS_NOTIFICATIONS_FILTER`] (`events.notifications.>`)
//! sees both in **stream-sequence order**, and `max_ack_pending = 1`
//! makes redelivery strictly in-order too: a skipped (un-acked) message
//! blocks the next from being delivered, so `ack → unack → re-ack` always
//! lands as the last event wins. A *second* consumer would race them —
//! hence one consumer, broadened filter (which forced the
//! [`CONSUMER_NAME`] rename + full re-projection, since a durable's filter
//! can't change in place).
//!
//! Idempotency: the audit log's `INSERT ... ON CONFLICT DO NOTHING` keys
//! on `(recipient, kind, occurred_at)` so redelivery / re-projection is a
//! no-op; the read-model UPSERT is naturally idempotent (same event ⇒
//! same resulting row).

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::ipc::notifications::{NotificationAcked, NotificationUnacked};
use kanade_shared::kv::STREAM_EVENTS;
use kanade_shared::subject::EVENTS_NOTIFICATIONS_FILTER;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

// pub(crate): consumer_reset::reset_if_wiped names this durable when
// deciding what to drop after a projection-DB wipe (#389). Renamed (`_v2`)
// when the filter broadened from `acked.>` to `notifications.>` — a
// durable consumer's filter_subject is immutable, so a new name + full
// re-projection is the migration path. The old `_projector` durable goes
// inert and can be deleted out of band.
pub(crate) const CONSUMER_NAME: &str = "backend_notification_acks_projector_v2";

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
                filter_subject: EVENTS_NOTIFICATIONS_FILTER.into(),
                // Strict in-order delivery: never hand us message N+1 while
                // N is unacked. With the skip-ack-on-error retry below this
                // guarantees a failed ack can't be overtaken by a later
                // unack for the same recipient. Notifications are
                // operator-volume, so the throughput cost is irrelevant.
                max_ack_pending: 1,
                ..Default::default()
            },
        )
        .await
        .context("create notification-acks consumer")?;
    info!(
        stream = STREAM_EVENTS,
        consumer = CONSUMER_NAME,
        filter = EVENTS_NOTIFICATIONS_FILTER,
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
        let subject = msg.subject.as_str();
        let is_unack = is_unack_subject(subject);
        let outcome = if is_unack {
            match serde_json::from_slice::<NotificationUnacked>(&msg.payload) {
                Ok(u) => apply_unack(&pool, &u, recorded_at).await.map(|_| {
                    debug!(
                        notification_id = %u.notification_id,
                        pc_id = %u.pc_id,
                        user_sid = %u.user_sid,
                        "projected notification unack",
                    );
                }),
                // A parse failure acks DELIBERATELY (warn + drop the one event),
                // it does NOT skip-ack like a projection error below. A
                // malformed payload is non-transient — the same bytes re-fail
                // forever — so with max_ack_pending=1 refusing to ack would
                // redeliver it ahead of every later message and wedge the whole
                // projector on a poison pill. Events come from our own agent, so
                // this should never fire; the warn is the alarm if it does.
                Err(e) => {
                    warn!(error = %e, %subject, "deserialize NotificationUnacked — dropping (deliberate ack)");
                    Ok(())
                }
            }
        } else {
            match serde_json::from_slice::<NotificationAcked>(&msg.payload) {
                Ok(a) => apply_ack(&pool, &a, recorded_at).await.map(|_| {
                    debug!(
                        notification_id = %a.notification_id,
                        pc_id = %a.pc_id,
                        user_sid = %a.user_sid,
                        "projected notification ack",
                    );
                }),
                // Deliberate ack-and-drop on a parse failure — see the unack
                // arm above for why (poison-pill avoidance under
                // max_ack_pending=1).
                Err(e) => {
                    warn!(error = %e, %subject, "deserialize NotificationAcked — dropping (deliberate ack)");
                    Ok(())
                }
            }
        };
        if let Err(err) = outcome {
            warn!(
                error = %err,
                %subject,
                "notification ack/unack projection failed — skipping ack so JetStream redelivers",
            );
            // Skip ack so JetStream redelivers. Unlike a parse failure (dropped
            // above), an apply error is transient (SQLite-busy etc.) so it IS
            // worth retrying. max_ack_pending=1 keeps redelivery strictly before
            // the next message, preserving order.
            continue;
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack notification-acks message");
        }
    }
    Ok(())
}

/// Classify an `events.notifications.*` subject as ack vs unack by its
/// FIXED event-type prefix — never a `contains` on the dynamic tail.
/// `notif_id` may contain dots (see the agent's `valid_notification_id`),
/// so an *acked* subject for an id like `my.unacked.notification` would
/// embed the substring `.unacked.`; a `contains` check would misclassify
/// that real ack as an unack, fail to deserialize it (no `unacked_at`
/// field), and silently drop the confirmation. The prefix up to and
/// including the event type can't be spoofed by later segments.
fn is_unack_subject(subject: &str) -> bool {
    subject.starts_with("events.notifications.unacked.")
}

/// Project an ack: append it to the audit log and UPSERT the read-model
/// row to the confirmed state (clearing any prior `unacked_at`, so a
/// re-ack after a retract correctly stands again). Both writes share one
/// transaction so the audit log and read model never disagree.
async fn apply_ack(
    pool: &SqlitePool,
    a: &NotificationAcked,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    append_event(
        &mut tx,
        &AckEventRow {
            notification_id: &a.notification_id,
            pc_id: &a.pc_id,
            user_sid: &a.user_sid,
            kind: "acked",
            occurred_at: a.acked_at,
            recorded_at,
            account: a.account.as_deref(),
        },
    )
    .await?;
    // Confirmed state. ON CONFLICT updates acked_at to this event's instant
    // and clears unacked_at — last ack in stream order wins, and a re-ack
    // after a retract returns the recipient to 確認済み.
    sqlx::query(
        "INSERT INTO notification_acks (
             notification_id, pc_id, user_sid, acked_at, recorded_at, account, unacked_at
         ) VALUES (?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT(notification_id, pc_id, user_sid) DO UPDATE SET
             acked_at    = excluded.acked_at,
             recorded_at = excluded.recorded_at,
             account     = excluded.account,
             unacked_at  = NULL",
    )
    .bind(&a.notification_id)
    .bind(&a.pc_id)
    .bind(&a.user_sid)
    .bind(a.acked_at)
    .bind(recorded_at)
    .bind(&a.account)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Project an unack: append it to the audit log and stamp `unacked_at` on
/// the read-model row (flipping the recipient to 取消済み). The UPDATE is
/// a deliberate no-op when no standing ack row exists (an orphan unack —
/// e.g. the ack aged out of the EVENTS window during re-projection); the
/// audit row is still recorded for completeness.
async fn apply_unack(
    pool: &SqlitePool,
    u: &NotificationUnacked,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    append_event(
        &mut tx,
        &AckEventRow {
            notification_id: &u.notification_id,
            pc_id: &u.pc_id,
            user_sid: &u.user_sid,
            kind: "unacked",
            occurred_at: u.unacked_at,
            recorded_at,
            account: u.account.as_deref(),
        },
    )
    .await?;
    // `AND acked_at <= ?unacked_at` is a last-event-wins guard. In normal
    // operation the single ordered consumer (max_ack_pending=1) already
    // serialises ack/unack, but a manual re-projection or out-of-order replay
    // could deliver a stale unack *after* a newer re-ack has advanced
    // acked_at. An unack always happens after the ack it cancels, so if the
    // row's standing acked_at is already newer than this unack's instant, the
    // unack belongs to a superseded confirmation and must NOT stamp the row —
    // otherwise it would corrupt a re-confirmed recipient back to 取消済み.
    // The audit log keeps the unack regardless (appended above).
    sqlx::query(
        "UPDATE notification_acks SET unacked_at = ?
         WHERE notification_id = ? AND pc_id = ? AND user_sid = ?
           AND acked_at <= ?",
    )
    .bind(u.unacked_at)
    .bind(&u.notification_id)
    .bind(&u.pc_id)
    .bind(&u.user_sid)
    .bind(u.unacked_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// One row for the [`append_event`] audit insert — groups the seven
/// logical fields of a lifecycle event so the function takes a single typed
/// argument instead of a long positional list.
struct AckEventRow<'a> {
    notification_id: &'a str,
    pc_id: &'a str,
    user_sid: &'a str,
    /// `"acked"` | `"unacked"`.
    kind: &'a str,
    /// Agent-stamped instant the user clicked (from the event body).
    occurred_at: chrono::DateTime<chrono::Utc>,
    /// JetStream publish time (re-projection-stable).
    recorded_at: chrono::DateTime<chrono::Utc>,
    account: Option<&'a str>,
}

/// Append one lifecycle event to the audit log. `INSERT ... ON CONFLICT DO
/// NOTHING` on `(recipient, kind, occurred_at)` makes redelivery / full
/// re-projection idempotent.
async fn append_event(tx: &mut sqlx::SqliteConnection, event: &AckEventRow<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO notification_ack_events (
             notification_id, pc_id, user_sid, kind, occurred_at, recorded_at, account
         ) VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(notification_id, pc_id, user_sid, kind, occurred_at) DO NOTHING",
    )
    .bind(event.notification_id)
    .bind(event.pc_id)
    .bind(event.user_sid)
    .bind(event.kind)
    .bind(event.occurred_at)
    .bind(event.recorded_at)
    .bind(event.account)
    .execute(&mut *tx)
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

    fn sample_unack(notif: &str, pc: &str, sid: &str, secs: u32) -> NotificationUnacked {
        NotificationUnacked {
            notification_id: notif.into(),
            pc_id: pc.into(),
            user_sid: sid.into(),
            unacked_at: Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, secs).unwrap(),
            account: Some("EXAMPLE\\taro".into()),
        }
    }

    /// Read one read-model row's (acked_at, unacked_at) — None if no row.
    async fn ack_state(
        pool: &SqlitePool,
        notif: &str,
        sid: &str,
    ) -> Option<(
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> {
        sqlx::query_as(
            "SELECT acked_at, unacked_at FROM notification_acks
             WHERE notification_id = ? AND user_sid = ?",
        )
        .bind(notif)
        .bind(sid)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn ack_insert_persists_confirmed_row() {
        let pool = fresh_pool().await;
        apply_ack(&pool, &sample("n1", "pc1", "S-1-5-21-1001"), Utc::now())
            .await
            .unwrap();
        let (_acked, unacked) = ack_state(&pool, "n1", "S-1-5-21-1001").await.unwrap();
        assert!(unacked.is_none(), "fresh ack ⇒ 確認済み (unacked_at NULL)");
        let acct: (Option<String>,) =
            sqlx::query_as("SELECT account FROM notification_acks WHERE notification_id = ?")
                .bind("n1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(acct.0.as_deref(), Some("EXAMPLE\\taro"));
    }

    #[tokio::test]
    async fn ack_redelivery_is_idempotent() {
        let pool = fresh_pool().await;
        let a = sample("n1", "pc1", "S-1-5-21-1001");
        apply_ack(&pool, &a, Utc::now()).await.unwrap();
        apply_ack(&pool, &a, Utc::now()).await.unwrap();
        // One read-model row, and the audit log collapses the duplicate
        // (same recipient+kind+occurred_at).
        let acks: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM notification_acks WHERE notification_id = ?")
                .bind("n1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(acks.0, 1);
        let events: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM notification_ack_events WHERE notification_id = ?",
        )
        .bind("n1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events.0, 1, "duplicate ack event deduped in audit log");
    }

    #[tokio::test]
    async fn unack_flips_to_revoked_but_keeps_acked_at_and_audits_both() {
        let pool = fresh_pool().await;
        apply_ack(&pool, &sample("n1", "pc1", "S-1-5-21-1001"), Utc::now())
            .await
            .unwrap();
        apply_unack(
            &pool,
            &sample_unack("n1", "pc1", "S-1-5-21-1001", 30),
            Utc::now(),
        )
        .await
        .unwrap();

        let (acked, unacked) = ack_state(&pool, "n1", "S-1-5-21-1001").await.unwrap();
        assert_eq!(
            acked,
            Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 5).unwrap(),
            "acked_at retained after revoke (so the operator sees they had confirmed)"
        );
        assert_eq!(
            unacked,
            Some(Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 30).unwrap()),
            "unacked_at stamped ⇒ 取消済み"
        );

        // Audit log holds BOTH the ack and the unack, in order.
        let kinds: Vec<(String,)> = sqlx::query_as(
            "SELECT kind FROM notification_ack_events
             WHERE notification_id = ? ORDER BY occurred_at",
        )
        .bind("n1")
        .fetch_all(&pool)
        .await
        .unwrap();
        let kinds: Vec<&str> = kinds.iter().map(|k| k.0.as_str()).collect();
        assert_eq!(kinds, vec!["acked", "unacked"]);
    }

    #[tokio::test]
    async fn reack_after_unack_returns_to_confirmed() {
        // ack → unack → re-ack: the read model must end 確認済み with the
        // newest acked_at and a cleared unacked_at (last event in stream
        // order wins), while the audit log keeps all three.
        let pool = fresh_pool().await;
        apply_ack(&pool, &sample("n1", "pc1", "S-1-5-21-1001"), Utc::now())
            .await
            .unwrap();
        apply_unack(
            &pool,
            &sample_unack("n1", "pc1", "S-1-5-21-1001", 30),
            Utc::now(),
        )
        .await
        .unwrap();
        let mut reack = sample("n1", "pc1", "S-1-5-21-1001");
        reack.acked_at = Utc.with_ymd_and_hms(2026, 5, 20, 12, 1, 0).unwrap();
        apply_ack(&pool, &reack, Utc::now()).await.unwrap();

        let (acked, unacked) = ack_state(&pool, "n1", "S-1-5-21-1001").await.unwrap();
        assert_eq!(acked, Utc.with_ymd_and_hms(2026, 5, 20, 12, 1, 0).unwrap());
        assert!(
            unacked.is_none(),
            "re-ack clears the revoke ⇒ 確認済み again"
        );
        let events: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM notification_ack_events WHERE notification_id = ?",
        )
        .bind("n1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events.0, 3, "audit log keeps ack + unack + re-ack");
    }

    #[tokio::test]
    async fn stale_unack_does_not_overwrite_a_newer_reack() {
        // Out-of-order / re-projection guard: a standing ack at 12:01:00, then
        // a STALE unack whose instant (12:00:30) predates it — e.g. a redelivery
        // of an old unack after a re-ack already advanced acked_at. The read
        // model must stay 確認済み (the unack belongs to a superseded
        // confirmation), even though the audit log still records it.
        let pool = fresh_pool().await;
        let mut standing = sample("n1", "pc1", "S-1-5-21-1001");
        standing.acked_at = Utc.with_ymd_and_hms(2026, 5, 20, 12, 1, 0).unwrap();
        apply_ack(&pool, &standing, Utc::now()).await.unwrap();

        // unacked_at = 12:00:30 < acked_at 12:01:00 ⇒ guarded out.
        apply_unack(
            &pool,
            &sample_unack("n1", "pc1", "S-1-5-21-1001", 30),
            Utc::now(),
        )
        .await
        .unwrap();

        let (acked, unacked) = ack_state(&pool, "n1", "S-1-5-21-1001").await.unwrap();
        assert_eq!(acked, Utc.with_ymd_and_hms(2026, 5, 20, 12, 1, 0).unwrap());
        assert!(
            unacked.is_none(),
            "stale unack must not flip a newer standing ack to 取消済み"
        );
        // ...but it is still in the audit log.
        let events: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM notification_ack_events
             WHERE notification_id = ? AND kind = 'unacked'",
        )
        .bind("n1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events.0, 1, "stale unack still audited");
    }

    #[tokio::test]
    async fn orphan_unack_is_noop_on_read_model_but_audited() {
        // An unack with no standing ack row (e.g. the ack aged out of the
        // EVENTS window during re-projection) must not create a row, but is
        // still recorded in the audit log.
        let pool = fresh_pool().await;
        apply_unack(
            &pool,
            &sample_unack("n1", "pc1", "S-1-5-21-1001", 30),
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(
            ack_state(&pool, "n1", "S-1-5-21-1001").await.is_none(),
            "orphan unack creates no read-model row"
        );
        let events: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM notification_ack_events WHERE notification_id = ?",
        )
        .bind("n1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events.0, 1, "orphan unack still audited");
    }

    #[test]
    fn unack_subject_discrimination_is_prefix_based() {
        // Real unack subject ⇒ classified as unack.
        assert!(is_unack_subject(
            "events.notifications.unacked.PC1.S-1-5-21-1.notif-9f3a"
        ));
        // Real ack subject ⇒ NOT unack.
        assert!(!is_unack_subject(
            "events.notifications.acked.PC1.S-1-5-21-1.notif-9f3a"
        ));
        // Regression (Gemini): an ACK whose notification id literally contains
        // ".unacked." must NOT be misread as an unack — a `contains` check
        // would have, silently dropping the confirmation.
        assert!(!is_unack_subject(
            "events.notifications.acked.PC1.S-1-5-21-1.my.unacked.notification"
        ));
    }

    #[tokio::test]
    async fn distinct_users_on_same_pc_each_get_a_row() {
        // Fast User Switching: two users on one PC ack the same
        // notification. The user_sid in the PK keeps them apart.
        let pool = fresh_pool().await;
        apply_ack(&pool, &sample("n1", "pc1", "S-1-5-21-1001"), Utc::now())
            .await
            .unwrap();
        apply_ack(&pool, &sample("n1", "pc1", "S-1-5-21-1002"), Utc::now())
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
