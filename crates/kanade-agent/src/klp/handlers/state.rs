//! `state.*` method handlers (SPEC §2.12.5).
//!
//! - `state.snapshot` — return the latest cached snapshot from
//!   `klp::state::eval_loop`'s watch channel. No side effects;
//!   safe to call repeatedly.
//! - `state.subscribe` — spawn a forwarder task that awaits
//!   `watch::Receiver::changed()` and writes a `state.changed`
//!   notification onto the connection's `push_tx` for each tick.
//!   Returns the subscription id (SPEC §2.12.7's `sub-<ns>-<n>`
//!   form).
//! - `state.unsubscribe` — abort the named forwarder. Returns
//!   `NotFound` if the id doesn't match any live subscription.
//!
//! The actual push payload (`state.changed`) doesn't come through
//! this handler module — it's written by the forwarder task
//! directly: builds an `RpcNotification`, serialises it, and
//! sends it on the shared `push_tx` channel for the writer task
//! to dispatch.

use chrono::Utc;
use kanade_shared::ipc::envelope::RpcNotification;
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::method;
use kanade_shared::ipc::state::{
    StateChangedParams, StateSnapshot, StateSnapshotParams, StateSubscribeParams,
    StateSubscribeResult, StateUnsubscribeParams,
};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use super::super::connection::ConnectionState;
use super::system::HandlerResult;

/// `state.snapshot` — return the latest snapshot produced by the
/// background evaluator. Cheap: just `borrow().clone()` on the
/// watch channel.
pub fn handle_state_snapshot(
    conn: &ConnectionState,
    _params: StateSnapshotParams,
) -> HandlerResult<StateSnapshot> {
    Ok(conn.state_rx.borrow().clone())
}

/// `state.subscribe` — spawn a forwarder task for this connection
/// and register its `JoinHandle` so `state.unsubscribe` can abort
/// it.
///
/// The forwarder runs until any of:
/// - `state.unsubscribe` aborts it.
/// - The connection's `push_tx` is dropped (writer task exited /
///   connection closed).
/// - `state_rx.changed()` returns `Err` (sender dropped — listener
///   shutdown).
pub fn handle_state_subscribe(
    conn: &mut ConnectionState,
    _params: StateSubscribeParams,
) -> HandlerResult<StateSubscribeResult> {
    let state_rx = conn.state_rx.clone();
    let push_tx = conn.push_tx.clone();
    let pc_id = conn.pc_id.clone();
    let handle = tokio::spawn(forward_state_changes(state_rx, push_tx, pc_id));
    let id = conn.subscriptions.register("s", handle);
    Ok(StateSubscribeResult { subscription: id })
}

/// `state.unsubscribe` — abort the named forwarder task. Returns
/// [`ErrorKind::NotFound`] when the id doesn't match a live
/// subscription (already cancelled, never issued, etc.).
pub fn handle_state_unsubscribe(
    conn: &mut ConnectionState,
    params: StateUnsubscribeParams,
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

/// Forwarder task body. Awaits each `state_rx.changed()`,
/// snapshots the current value, builds a `state.changed`
/// notification, and tries to push it into `push_tx`. Quits
/// silently when either channel is closed (the connection or
/// the agent is shutting down).
async fn forward_state_changes(
    mut state_rx: watch::Receiver<StateSnapshot>,
    push_tx: mpsc::Sender<Vec<u8>>,
    pc_id: String,
) {
    debug!(pc_id = %pc_id, "state forwarder: subscribed");
    // We DON'T push the initial value — the client just called
    // `state.snapshot` immediately before subscribing per the
    // SPEC §2.12.8 example, so they already have the current
    // state.
    while state_rx.changed().await.is_ok() {
        let snapshot = state_rx.borrow().clone();
        let params = StateChangedParams {
            snapshot,
            at: Utc::now(),
        };
        let notif = match RpcNotification::new(method::STATE_CHANGED, &params) {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "state forwarder: failed to encode notification");
                continue;
            }
        };
        let body = match serde_json::to_vec(&notif) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "state forwarder: failed to serialise frame");
                continue;
            }
        };
        if push_tx.send(body).await.is_err() {
            // Writer task exited — connection is closing. Exit
            // cleanly; the SubscriptionRegistry's Drop will
            // also have aborted us by now.
            debug!(pc_id = %pc_id, "state forwarder: push channel closed, exiting");
            return;
        }
    }
    debug!(pc_id = %pc_id, "state forwarder: state_rx closed (eval_loop shutdown)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klp::auth::PeerCredentials;
    use kanade_shared::ipc::envelope::RpcMessage;
    use kanade_shared::wire::EffectiveConfig;
    use std::path::PathBuf;
    use std::time::Duration;

    fn dummy_snapshot(version: &str) -> StateSnapshot {
        StateSnapshot {
            pc_id: "PC1234".into(),
            online: true,
            vpn: "unknown".into(),
            checks: vec![],
            agent_version: version.into(),
            target_version: version.into(),
        }
    }

    fn fresh_conn_with(
        state_tx: &watch::Sender<StateSnapshot>,
        push_tx: mpsc::Sender<Vec<u8>>,
    ) -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.41.0".into(),
            cfg_rx,
            state_tx.subscribe(),
            PathBuf::from("agent.log"),
            push_tx,
        )
    }

    #[tokio::test]
    async fn snapshot_returns_cached_value() {
        let (state_tx, _) = watch::channel(dummy_snapshot("0.41.0"));
        let (push_tx, _push_rx) = mpsc::channel(8);
        let conn = fresh_conn_with(&state_tx, push_tx);
        let snap = handle_state_snapshot(&conn, StateSnapshotParams::default()).unwrap();
        assert_eq!(snap.agent_version, "0.41.0");
        assert!(snap.online);
    }

    #[tokio::test]
    async fn subscribe_returns_sub_s_n_id_and_registers_forwarder() {
        let (state_tx, _) = watch::channel(dummy_snapshot("0.41.0"));
        let (push_tx, _push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn_with(&state_tx, push_tx);

        let r1 = handle_state_subscribe(&mut conn, StateSubscribeParams::default()).unwrap();
        let r2 = handle_state_subscribe(&mut conn, StateSubscribeParams::default()).unwrap();
        assert_eq!(r1.subscription, "sub-s-1");
        assert_eq!(r2.subscription, "sub-s-2");
        // Both forwarders are registered.
        assert_eq!(conn.subscriptions.len(), 2);
    }

    #[tokio::test]
    async fn subscribed_forwarder_pushes_state_changed_on_watch_tick() {
        let (state_tx, _) = watch::channel(dummy_snapshot("0.41.0"));
        let (push_tx, mut push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn_with(&state_tx, push_tx);

        let _ = handle_state_subscribe(&mut conn, StateSubscribeParams::default()).unwrap();

        // Trigger a state change.
        state_tx.send(dummy_snapshot("0.42.0")).unwrap();

        // Forwarder should pump a `state.changed` notification
        // into push_rx. Give it a generous timeout — the test
        // shouldn't be flaky, but tokio scheduling on busy CI
        // boxes can blip.
        let body = tokio::time::timeout(Duration::from_secs(1), push_rx.recv())
            .await
            .expect("forwarder should push within 1s")
            .expect("push_tx still open");

        let msg: RpcMessage = serde_json::from_slice(&body).expect("decode wire frame");
        match msg {
            RpcMessage::Notification(n) => {
                assert_eq!(n.method, method::STATE_CHANGED);
                // The flatten attribute means the payload sits at
                // the top level of `params`.
                let params: StateChangedParams =
                    serde_json::from_value(n.params).expect("decode StateChangedParams");
                assert_eq!(params.snapshot.agent_version, "0.42.0");
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unsubscribe_aborts_forwarder_and_returns_ok() {
        let (state_tx, _) = watch::channel(dummy_snapshot("0.41.0"));
        let (push_tx, mut push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn_with(&state_tx, push_tx);
        let r = handle_state_subscribe(&mut conn, StateSubscribeParams::default()).unwrap();
        assert_eq!(conn.subscriptions.len(), 1);

        handle_state_unsubscribe(
            &mut conn,
            StateUnsubscribeParams {
                subscription: r.subscription.clone(),
            },
        )
        .expect("unsubscribe should succeed");
        assert_eq!(conn.subscriptions.len(), 0);

        // After unsubscribe, a state tick should NOT push.
        state_tx.send(dummy_snapshot("0.42.0")).unwrap();
        let res = tokio::time::timeout(Duration::from_millis(200), push_rx.recv()).await;
        assert!(
            res.is_err(),
            "expected timeout (no push), got: {:?}",
            res.unwrap()
        );
    }

    #[tokio::test]
    async fn unsubscribe_unknown_id_returns_not_found() {
        let (state_tx, _) = watch::channel(dummy_snapshot("0.41.0"));
        let (push_tx, _) = mpsc::channel(8);
        let mut conn = fresh_conn_with(&state_tx, push_tx);
        let err = handle_state_unsubscribe(
            &mut conn,
            StateUnsubscribeParams {
                subscription: "sub-s-999".into(),
            },
        )
        .expect_err("unknown id must error");
        let data = err.data.expect("data populated");
        assert_eq!(data.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn forwarder_does_not_push_initial_value_before_first_change() {
        // SPEC §2.12.8 has the client call `state.snapshot` then
        // `state.subscribe` — so the subscriber already knows
        // the current state. Don't double-push.
        let (state_tx, _) = watch::channel(dummy_snapshot("0.41.0"));
        let (push_tx, mut push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn_with(&state_tx, push_tx);

        let _ = handle_state_subscribe(&mut conn, StateSubscribeParams::default()).unwrap();

        // No state change yet → no push expected.
        let res = tokio::time::timeout(Duration::from_millis(200), push_rx.recv()).await;
        assert!(res.is_err(), "forwarder must not push on subscribe alone");
    }
}
