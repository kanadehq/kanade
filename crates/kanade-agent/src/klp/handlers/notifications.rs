//! `notifications.*` method handlers (SPEC §2.12.5 — Phase E, live
//! push half).
//!
//! - `notifications.subscribe` — spawn a forwarder task that awaits the
//!   agent-wide notification broadcast ([`crate::klp::notify_bus`]) and
//!   writes a `notifications.new` push onto this connection's `push_tx`
//!   for each incoming notification. Returns the subscription id
//!   (`sub-n-<n>`).
//! - `notifications.unsubscribe` — abort the named forwarder.
//! - `notifications.ack` — write the per-user read mark into the
//!   `notifications_read` KV and publish the
//!   `events.notifications.acked.>` event the backend projects into the
//!   operator's confirmation view.
//!
//! `notifications.list` (history replay) lands in a follow-up PR.
//!
//! Mirrors the `state.*` forwarder shape, but the source is a
//! `broadcast::Receiver<Notification>` (discrete events) instead of a
//! `watch::Receiver` (latest-state) — so the forwarder handles
//! `RecvError::Lagged` (a slow client that fell behind; tokio advances
//! the cursor to the oldest still-buffered message, so delivery resumes
//! there and works forward) and `RecvError::Closed` (the bus exited).

use chrono::Utc;
use kanade_shared::ipc::envelope::RpcNotification;
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::method;
use kanade_shared::ipc::notifications::{
    Notification, NotificationAcked, NotificationNewParams, NotificationsAckParams,
    NotificationsAckResult, NotificationsSubscribeParams, NotificationsSubscribeResult,
    NotificationsUnsubscribeParams,
};
use kanade_shared::kv::{BUCKET_NOTIFICATIONS_READ, notifications_read_key};
use kanade_shared::subject;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::super::connection::ConnectionState;
use super::system::HandlerResult;

/// `notifications.subscribe` — start streaming `notifications.new`
/// pushes for this connection. Derives a fresh broadcast receiver from
/// the agent-wide bus and registers the forwarder so
/// `notifications.unsubscribe` can abort it.
pub fn handle_notifications_subscribe(
    conn: &mut ConnectionState,
    _params: NotificationsSubscribeParams,
) -> HandlerResult<NotificationsSubscribeResult> {
    let rx = conn.notif_subscribe().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "notification bus not available on this agent build",
        )
    })?;
    let push_tx = conn.push_tx.clone();
    let pc_id = conn.pc_id.clone();
    let handle = tokio::spawn(forward_notifications(rx, push_tx, pc_id));
    let id = conn.subscriptions.register("n", handle);
    Ok(NotificationsSubscribeResult { subscription: id })
}

/// `notifications.unsubscribe` — abort the named forwarder. Returns
/// [`ErrorKind::NotFound`] when the id doesn't match a live
/// subscription.
pub fn handle_notifications_unsubscribe(
    conn: &mut ConnectionState,
    params: NotificationsUnsubscribeParams,
) -> HandlerResult<()> {
    if conn.subscriptions.unsubscribe(&params.subscription) {
        Ok(())
    } else {
        Err(RpcError::new(
            ErrorKind::NotFound,
            format!("subscription '{}' not found", params.subscription),
        ))
    }
}

/// `notifications.ack` — record the caller's confirmation of one
/// notification (SPEC §2.12.4 / Phase E). Two side effects:
///
/// 1. Write the per-user read mark into the `notifications_read` KV
///    under `{pc_id}.{user_sid}.{notification_id}`, so
///    `notifications.list` can filter this user's unread set.
/// 2. Publish `events.notifications.acked.{pc_id}.{user_sid}.{notif_id}`
///    (an acknowledged JetStream publish) so the backend's
///    notification-acks projector records who confirmed when — that
///    feeds `GET /api/notifications/{id}/ack_status`.
///
/// The SID is the OS-derived [`ConnectionState::peer`] identity, never
/// a payload field (SPEC §2.12.4): a user can only ack as themselves,
/// even on a shared PC. A connection whose SID couldn't be resolved
/// (`"<unknown>"`) is rejected rather than writing a colliding row.
pub async fn handle_notifications_ack(
    conn: &ConnectionState,
    params: NotificationsAckParams,
) -> HandlerResult<NotificationsAckResult> {
    // Validate inputs before touching NATS so a bad request fails
    // cheaply (and so the guard paths are unit-testable without a
    // broker).
    let user_sid = conn.peer.user_sid.as_str();
    if user_sid.is_empty() || user_sid == "<unknown>" {
        return Err(RpcError::new(
            ErrorKind::InternalError,
            "caller SID could not be resolved; cannot record ack",
        ));
    }
    let notif_id = params.id.trim();
    if !valid_notification_id(notif_id) {
        // The id flows into a NATS KV key and the ack publish subject,
        // so an unvalidated id with NATS-special chars (space, `.`
        // beyond the allowed set, wildcards `*` / `>`, `/`) would be
        // rejected by the broker and surface as an opaque
        // InternalError. Reject up front with InvalidParams instead.
        return Err(RpcError::new(
            ErrorKind::InvalidParams,
            "notification id must be non-empty and contain only [A-Za-z0-9_.-]",
        ));
    }
    let client = conn.nats.as_ref().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "NATS client not available on this agent build",
        )
    })?;
    let pc_id = conn.pc_id.as_str();
    let acked_at = Utc::now();

    let js = async_nats::jetstream::new(client.clone());

    // 1. Persist the per-user read mark. Value matches SPEC §2.3.2:
    //    `{"acked_at": ..., "acked_by": "<sid>"}`.
    let kv = js
        .get_key_value(BUCKET_NOTIFICATIONS_READ)
        .await
        .map_err(|e| {
            RpcError::new(
                ErrorKind::InternalError,
                format!("open {BUCKET_NOTIFICATIONS_READ} KV: {e}"),
            )
        })?;
    let key = notifications_read_key(pc_id, user_sid, notif_id);
    let value = serde_json::to_vec(&serde_json::json!({
        "acked_at": acked_at,
        "acked_by": user_sid,
    }))
    .map_err(|e| RpcError::new(ErrorKind::InternalError, e.to_string()))?;
    kv.put(key, value.into()).await.map_err(|e| {
        RpcError::new(
            ErrorKind::InternalError,
            format!("write {BUCKET_NOTIFICATIONS_READ}: {e}"),
        )
    })?;

    // 2. Publish the ack event (acknowledged JetStream publish so a
    //    broker problem surfaces here instead of silently dropping the
    //    operator's confirmation view).
    let event = NotificationAcked {
        notification_id: notif_id.to_string(),
        pc_id: pc_id.to_string(),
        user_sid: user_sid.to_string(),
        acked_at,
    };
    let payload = serde_json::to_vec(&event)
        .map_err(|e| RpcError::new(ErrorKind::InternalError, e.to_string()))?;
    let subj = subject::events_notifications_acked(pc_id, user_sid, notif_id);
    let ack = js
        .publish(subj.clone(), payload.into())
        .await
        .map_err(|e| RpcError::new(ErrorKind::InternalError, format!("publish {subj}: {e}")))?;
    ack.await.map_err(|e| {
        RpcError::new(
            ErrorKind::InternalError,
            format!("ack publish to {subj} not confirmed: {e}"),
        )
    })?;

    info!(
        pc_id = %pc_id,
        user_sid = %user_sid,
        notification_id = %notif_id,
        "notification acked",
    );
    Ok(NotificationsAckResult { acked_at })
}

/// `notifications.ack` id charset gate. Same `[A-Za-z0-9_.-]` set as
/// `jobs::valid_job_id` (kept local so the two namespaces stay
/// decoupled) — these are the characters safe in both a NATS KV key
/// and a publish subject token, so a bad id is caught as
/// `InvalidParams` here rather than as an opaque broker error later.
fn valid_notification_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Forwarder task body. Awaits each broadcast notification, builds a
/// `notifications.new` push, and sends it on `push_tx`. Exits when the
/// connection's writer is gone (`push_tx` closed) or the bus shut down
/// (`Closed`). On `Lagged` (only reachable after a >256-deep backlog,
/// implausible for operator-initiated notifications) tokio drops the
/// missed span and advances to the oldest still-buffered message; the
/// loop logs the skip and resumes delivery from there.
async fn forward_notifications(
    mut rx: broadcast::Receiver<Notification>,
    push_tx: mpsc::Sender<Vec<u8>>,
    pc_id: String,
) {
    debug!(pc_id = %pc_id, "notifications forwarder: subscribed");
    loop {
        let notification = match rx.recv().await {
            Ok(n) => n,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    pc_id = %pc_id,
                    skipped,
                    "notifications forwarder: lagged; resuming at oldest buffered",
                );
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!(pc_id = %pc_id, "notifications forwarder: bus closed, exiting");
                return;
            }
        };
        let params = NotificationNewParams { notification };
        let notif = match RpcNotification::new(method::NOTIFICATIONS_NEW, &params) {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "notifications forwarder: failed to encode notification");
                continue;
            }
        };
        let body = match serde_json::to_vec(&notif) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "notifications forwarder: failed to serialise frame");
                continue;
            }
        };
        if push_tx.send(body).await.is_err() {
            debug!(pc_id = %pc_id, "notifications forwarder: push channel closed, exiting");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klp::auth::PeerCredentials;
    use kanade_shared::ipc::envelope::RpcMessage;
    use kanade_shared::ipc::notifications::NotificationPriority;
    use kanade_shared::ipc::state::StateSnapshot;
    use kanade_shared::wire::EffectiveConfig;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::watch;

    fn dummy_snapshot() -> StateSnapshot {
        StateSnapshot {
            pc_id: "PC1234".into(),
            online: true,
            vpn: "unknown".into(),
            checks: vec![],
            agent_version: "0.43.0".into(),
            target_version: "0.43.0".into(),
        }
    }

    fn sample_notification(id: &str) -> Notification {
        Notification {
            id: id.into(),
            priority: NotificationPriority::Emergency,
            require_ack: true,
            title: "緊急: ネットワーク機器メンテ".into(),
            body: "22時から30分停止します".into(),
            issued_at: chrono::Utc::now(),
            issued_by: Some("infra-team".into()),
            expires_at: None,
            acked_at: None,
        }
    }

    fn fresh_conn(
        notif_tx: &broadcast::Sender<Notification>,
        push_tx: mpsc::Sender<Vec<u8>>,
    ) -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let (_state_tx, state_rx) = watch::channel(dummy_snapshot());
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                user_sid: "S-1-5-21-1001".into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.43.0".into(),
            cfg_rx,
            state_rx,
            PathBuf::from("agent.log"),
            push_tx,
        )
        .with_notifications(notif_tx.clone())
    }

    #[tokio::test]
    async fn subscribe_returns_sub_n_id_and_registers_forwarder() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, _push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let r1 = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();
        let r2 = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();
        assert_eq!(r1.subscription, "sub-n-1");
        assert_eq!(r2.subscription, "sub-n-2");
        assert_eq!(conn.subscriptions.len(), 2);
    }

    #[tokio::test]
    async fn subscribed_forwarder_pushes_notifications_new() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, mut push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let _ = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();

        notif_tx.send(sample_notification("notif-9f3a")).unwrap();

        let body = tokio::time::timeout(Duration::from_secs(1), push_rx.recv())
            .await
            .expect("forwarder should push within 1s")
            .expect("push_tx still open");
        let msg: RpcMessage = serde_json::from_slice(&body).expect("decode frame");
        match msg {
            RpcMessage::Notification(n) => {
                assert_eq!(n.method, method::NOTIFICATIONS_NEW);
                let params: NotificationNewParams =
                    serde_json::from_value(n.params).expect("decode NotificationNewParams");
                assert_eq!(params.notification.id, "notif-9f3a");
                assert_eq!(
                    params.notification.priority,
                    NotificationPriority::Emergency
                );
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unsubscribe_aborts_forwarder() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, mut push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let r = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();
        assert_eq!(conn.subscriptions.len(), 1);

        handle_notifications_unsubscribe(
            &mut conn,
            NotificationsUnsubscribeParams {
                subscription: r.subscription,
            },
        )
        .expect("unsubscribe should succeed");
        assert_eq!(conn.subscriptions.len(), 0);

        // After unsubscribe a broadcast must not push.
        notif_tx.send(sample_notification("notif-2")).unwrap();
        let res = tokio::time::timeout(Duration::from_millis(200), push_rx.recv()).await;
        assert!(res.is_err(), "expected no push after unsubscribe");
    }

    #[tokio::test]
    async fn unsubscribe_unknown_id_returns_not_found() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, _) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let err = handle_notifications_unsubscribe(
            &mut conn,
            NotificationsUnsubscribeParams {
                subscription: "sub-n-999".into(),
            },
        )
        .expect_err("unknown id must error");
        assert_eq!(err.data.expect("data").kind, ErrorKind::NotFound);
    }

    /// Build a connection with an explicit SID and no NATS client, for
    /// exercising the `notifications.ack` input guards (which run before
    /// any broker access). The happy path needs a live broker and is
    /// covered by integration tests, not here.
    fn conn_for_ack(user_sid: &str) -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let (_state_tx, state_rx) = watch::channel(dummy_snapshot());
        let (push_tx, _push_rx) = mpsc::channel(8);
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                user_sid: user_sid.into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.43.0".into(),
            cfg_rx,
            state_rx,
            PathBuf::from("agent.log"),
            push_tx,
        )
    }

    #[test]
    fn valid_notification_id_accepts_ids_rejects_nats_unsafe() {
        for ok in ["notif-9f3a", "maintenance-2026-05-20", "a.b", "Job_123"] {
            assert!(valid_notification_id(ok), "{ok} should be valid");
        }
        for bad in ["", "has space", "wild*", "a>b", "with/slash", "qu?x"] {
            assert!(!valid_notification_id(bad), "{bad:?} should be invalid");
        }
    }

    #[tokio::test]
    async fn ack_blank_or_unsafe_id_returns_invalid_params() {
        let conn = conn_for_ack("S-1-5-21-1001");
        for bad in ["  ", "bad id", "wild*"] {
            let err = handle_notifications_ack(&conn, NotificationsAckParams { id: bad.into() })
                .await
                .expect_err("bad id must error");
            assert_eq!(
                err.data.expect("data").kind,
                ErrorKind::InvalidParams,
                "id {bad:?}",
            );
        }
    }

    #[tokio::test]
    async fn ack_unknown_sid_is_rejected() {
        // A connection whose SID couldn't be resolved must not write a
        // colliding `<unknown>` KV row.
        let conn = conn_for_ack("<unknown>");
        let err = handle_notifications_ack(
            &conn,
            NotificationsAckParams {
                id: "notif-1".into(),
            },
        )
        .await
        .expect_err("unknown SID must error");
        let data = err.data.expect("data");
        assert_eq!(data.kind, ErrorKind::InternalError);
        assert!(data.detail.contains("SID"), "detail: {}", data.detail);
    }

    #[tokio::test]
    async fn ack_without_nats_client_errors_internal() {
        // Valid SID + id, but the test connection has no NATS client
        // (conn_for_ack skips with_nats) — the handler reports an
        // internal error rather than panicking.
        let conn = conn_for_ack("S-1-5-21-1001");
        let err = handle_notifications_ack(
            &conn,
            NotificationsAckParams {
                id: "notif-1".into(),
            },
        )
        .await
        .expect_err("missing NATS client must error");
        assert_eq!(err.data.expect("data").kind, ErrorKind::InternalError);
    }
}
