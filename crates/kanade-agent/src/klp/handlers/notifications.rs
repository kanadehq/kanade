//! `notifications.*` method handlers (SPEC §2.12.5 — Phase E, live
//! push half).
//!
//! - `notifications.subscribe` — spawn a forwarder task that awaits the
//!   agent-wide notification broadcast ([`crate::klp::notify_bus`]) and
//!   writes a `notifications.new` push onto this connection's `push_tx`
//!   for each incoming notification. Returns the subscription id
//!   (`sub-n-<n>`).
//! - `notifications.unsubscribe` — abort the named forwarder.
//!
//! `notifications.list` (history replay) and `notifications.ack` land
//! in follow-up PRs; this PR wires the live-push path so an operator
//! send reaches a connected Client App.
//!
//! Mirrors the `state.*` forwarder shape, but the source is a
//! `broadcast::Receiver<Notification>` (discrete events) instead of a
//! `watch::Receiver` (latest-state) — so the forwarder handles
//! `RecvError::Lagged` (a slow client that fell behind; tokio advances
//! the cursor to the oldest still-buffered message, so delivery resumes
//! there and works forward) and `RecvError::Closed` (the bus exited).

use kanade_shared::ipc::envelope::RpcNotification;
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::method;
use kanade_shared::ipc::notifications::{
    Notification, NotificationNewParams, NotificationsSubscribeParams,
    NotificationsSubscribeResult, NotificationsUnsubscribeParams,
};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{debug, warn};

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
}
