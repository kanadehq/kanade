//! Per-connection KLP state.
//!
//! One [`ConnectionState`] is constructed when a client connects to
//! the listener; it lives for the duration of that single
//! connection and is dropped when the read loop exits. State that
//! must survive across reconnects (e.g. the operator audit log)
//! lives elsewhere — at the connection level we only track what's
//! locally relevant: the handshake gate, the agreed protocol
//! version, the peer's identity, the cheaply-cloneable globals
//! (config + state watches, log path, agent identity) handler
//! code needs to do its work without reaching into module state,
//! and the subscription registry + push channel that connect
//! handlers to the writer task.

use std::path::PathBuf;

use kanade_shared::ipc::handshake::HandshakeSession;
use kanade_shared::ipc::state::StateSnapshot;
use kanade_shared::wire::EffectiveConfig;
use tokio::sync::{mpsc, watch};

use super::auth::PeerCredentials;
use super::subscriptions::SubscriptionRegistry;

/// State shared across handlers for a single open KLP connection.
///
/// `pre-handshake` ⇒ `agreed_protocol` is `None`; every method
/// except `system.handshake` returns
/// [`kanade_shared::ipc::error::ErrorKind::InvalidRequest`].
/// `post-handshake` ⇒ `agreed_protocol` is `Some(v)` and the
/// dispatcher accepts the full method set.
///
/// `peer` is captured at connect time from the OS — never from
/// the payload — per SPEC §2.12.4.
pub struct ConnectionState {
    /// Peer SID + session id derived from
    /// [`super::auth::resolve_peer`]. Authoritative identity for
    /// every audit-log entry on this connection.
    pub peer: PeerCredentials,
    /// Agent's configured `pc_id`. Plumbed in so handlers don't
    /// reach into global state to find it.
    pub pc_id: String,
    /// Currently-running agent binary version
    /// (`CARGO_PKG_VERSION`). Returned in handshake + ping
    /// responses without needing each handler to import the
    /// crate-root constant.
    pub agent_version: String,
    /// Live view of the agent's effective config (Sprint 6's
    /// config supervisor). Handlers needing the rollout target
    /// version, etc. read `config_rx.borrow()` per call —
    /// cheap because [`watch::Receiver`] is just an Arc-shared
    /// slot.
    pub config_rx: watch::Receiver<EffectiveConfig>,
    /// Live view of the latest endpoint state snapshot produced
    /// by `klp::state::eval_loop`. `state.snapshot` returns
    /// `borrow().clone()`; `state.subscribe` spawns a forwarder
    /// that awaits `changed()` and pushes a `state.changed`
    /// notification per tick.
    pub state_rx: watch::Receiver<StateSnapshot>,
    /// On-disk path to the rotating agent.log file. Used by
    /// `system.log_tail` to bundle recent log lines into a
    /// support response.
    pub log_path: PathBuf,
    /// Tracks every push-stream subscription this connection has
    /// opened. The `Drop` impl aborts all outstanding forwarder
    /// tasks so a careless client (no explicit unsubscribe)
    /// doesn't leak tasks.
    pub subscriptions: SubscriptionRegistry,
    /// Shared channel into the connection's writer task.
    /// Handlers AND subscription forwarders both send encoded
    /// frames here; the writer task drains and writes to the
    /// pipe. Bounded so a misbehaving forwarder can't OOM the
    /// agent.
    pub push_tx: mpsc::Sender<Vec<u8>>,
    /// NATS client for cold-path KV reads. `jobs.list` opens
    /// `BUCKET_JOBS` on demand to re-read the manifest catalog at
    /// fire time (SPEC §2.1) rather than caching a stale snapshot.
    /// `None` only in unit tests that don't drive a KV-backed handler;
    /// production wires it via [`ConnectionState::with_nats`].
    pub nats: Option<async_nats::Client>,
    /// `Some(v)` once `system.handshake` succeeded; `None`
    /// otherwise. The dispatcher uses this as the gate for
    /// non-handshake methods.
    agreed_protocol: Option<u32>,
}

impl ConnectionState {
    /// Build the pre-handshake state for a freshly-accepted
    /// connection. The caller (the listener task) clones cheap
    /// globals — `config_rx` / `state_rx` via
    /// [`watch::Receiver::clone`], `log_path` + identity strings
    /// via owned values — into the fresh state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        peer: PeerCredentials,
        pc_id: String,
        agent_version: String,
        config_rx: watch::Receiver<EffectiveConfig>,
        state_rx: watch::Receiver<StateSnapshot>,
        log_path: PathBuf,
        push_tx: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        Self {
            peer,
            pc_id,
            agent_version,
            config_rx,
            state_rx,
            log_path,
            subscriptions: SubscriptionRegistry::new(),
            push_tx,
            nats: None,
            agreed_protocol: None,
        }
    }

    /// Attach the NATS client used by cold-path KV handlers
    /// (`jobs.list`). The listener calls this on every accepted
    /// connection; unit tests that don't exercise a KV handler skip
    /// it and leave `nats` as `None`.
    pub fn with_nats(mut self, nats: async_nats::Client) -> Self {
        self.nats = Some(nats);
        self
    }

    /// `true` once `system.handshake` has been processed
    /// successfully on this connection.
    pub fn handshake_complete(&self) -> bool {
        self.agreed_protocol.is_some()
    }

    /// Mark the handshake as complete and record the agreed
    /// protocol version. Idempotent: re-handshake on the same
    /// connection just overwrites — SPEC §2.12.6 doesn't forbid
    /// it explicitly, and treating it as a no-op error would
    /// complicate the dispatcher without buying anything.
    pub fn mark_handshake(&mut self, protocol: u32) {
        self.agreed_protocol = Some(protocol);
    }

    /// Build the `session` block returned in the handshake
    /// response. Derived from `peer` + `pc_id`, never from
    /// payload (SPEC §2.12.4 — "Agent は OS 由来の SID/UID を真と
    /// みなす").
    pub fn session(&self) -> HandshakeSession {
        HandshakeSession {
            user: self.peer.user.clone(),
            session_id: self.peer.session_id,
            pc_id: self.pc_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_peer() -> PeerCredentials {
        PeerCredentials {
            user: "DOMAIN\\alice".into(),
            session_id: 2,
        }
    }

    fn dummy_snapshot() -> StateSnapshot {
        StateSnapshot {
            pc_id: "PC1234".into(),
            online: true,
            vpn: "unknown".into(),
            checks: vec![],
            agent_version: "0.41.0".into(),
            target_version: "0.41.0".into(),
        }
    }

    fn fresh_state() -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let (_state_tx, state_rx) = watch::channel(dummy_snapshot());
        let (push_tx, _push_rx) = mpsc::channel(8);
        ConnectionState::new(
            dummy_peer(),
            "PC1234".into(),
            "0.41.0".into(),
            cfg_rx,
            state_rx,
            PathBuf::from("agent.log"),
            push_tx,
        )
    }

    #[tokio::test]
    async fn fresh_connection_is_pre_handshake() {
        let s = fresh_state();
        assert!(!s.handshake_complete());
    }

    #[tokio::test]
    async fn mark_handshake_unlocks_method_set() {
        let mut s = fresh_state();
        s.mark_handshake(1);
        assert!(s.handshake_complete());
    }

    #[tokio::test]
    async fn session_uses_authoritative_os_identity() {
        // Critical invariant per SPEC §2.12.4: the session that
        // ships back to the client is built from the peer (OS) and
        // the agent's pc_id, never from any payload field.
        let s = fresh_state();
        let session = s.session();
        assert_eq!(session.user, "DOMAIN\\alice");
        assert_eq!(session.session_id, 2);
        assert_eq!(session.pc_id, "PC1234");
    }
}
