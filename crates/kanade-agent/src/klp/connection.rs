//! Per-connection KLP state.
//!
//! One [`ConnectionState`] is constructed when a client connects to
//! the listener; it lives for the duration of that single
//! connection and is dropped when the read loop exits. State that
//! must survive across reconnects (e.g. the operator audit log)
//! lives elsewhere — at the connection level we only track what's
//! locally relevant: the handshake gate, the agreed protocol
//! version, the peer's identity, and (in a later PR)
//! per-connection subscription handles.

use kanade_shared::ipc::handshake::HandshakeSession;

use super::auth::PeerCredentials;

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
    /// `Some(v)` once `system.handshake` succeeded; `None`
    /// otherwise. The dispatcher uses this as the gate for
    /// non-handshake methods.
    agreed_protocol: Option<u32>,
}

impl ConnectionState {
    /// Build the pre-handshake state for a freshly-accepted
    /// connection.
    pub fn new(peer: PeerCredentials, pc_id: String, agent_version: String) -> Self {
        Self {
            peer,
            pc_id,
            agent_version,
            agreed_protocol: None,
        }
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

    #[test]
    fn fresh_connection_is_pre_handshake() {
        let s = ConnectionState::new(dummy_peer(), "PC1234".into(), "0.40.0".into());
        assert!(!s.handshake_complete());
    }

    #[test]
    fn mark_handshake_unlocks_method_set() {
        let mut s = ConnectionState::new(dummy_peer(), "PC1234".into(), "0.40.0".into());
        s.mark_handshake(1);
        assert!(s.handshake_complete());
    }

    #[test]
    fn session_uses_authoritative_os_identity() {
        // Critical invariant per SPEC §2.12.4: the session that
        // ships back to the client is built from the peer (OS) and
        // the agent's pc_id, never from any payload field.
        let s = ConnectionState::new(dummy_peer(), "PC1234".into(), "0.40.0".into());
        let session = s.session();
        assert_eq!(session.user, "DOMAIN\\alice");
        assert_eq!(session.session_id, 2);
        assert_eq!(session.pc_id, "PC1234");
    }
}
