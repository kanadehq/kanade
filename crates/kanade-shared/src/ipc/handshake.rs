//! `system.handshake` types (SPEC §2.12.6).
//!
//! Every KLP connection's first request. Negotiates the protocol
//! version and the agent's optional features, and returns the
//! OS-derived session info (user SID + console session id + pc_id)
//! the client uses to label its UI and audit-log entries.
//!
//! Until handshake completes, the agent rejects every other method
//! with [`super::error::ErrorKind::InvalidRequest`] (SPEC §2.12.6
//! "Handshake 未完了の状態で他 method を呼ぶと -32600 InvalidRequest").

use serde::{Deserialize, Serialize};

/// The single protocol version KLP v1 ships with. Listed in
/// [`HandshakeParams::protocol`] by the client; the agent picks the
/// highest mutually-supported version into
/// [`HandshakeResult::protocol`]. If no overlap, the agent returns
/// [`super::error::ErrorKind::StaleProtocol`].
pub const PROTOCOL_V1: u32 = 1;

/// `system.handshake` request params.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct HandshakeParams {
    /// Client identifier (e.g. `"kanade-client"`). Free-form, used
    /// for the audit log + tracing spans. Agent does not gate on
    /// this — alternate clients (test harnesses, ops CLI) are fine.
    pub client: String,
    /// Client binary version (semver). Surfaced into the audit log.
    pub client_version: String,
    /// All protocol versions the client can speak. The agent picks
    /// the highest mutually-supported version into
    /// [`HandshakeResult::protocol`]. Length must be ≥ 1 — an empty
    /// array returns `InvalidParams`.
    pub protocol: Vec<u32>,
    /// Optional features the client wants to use. Agent answers
    /// with its own [`HandshakeResult::features`] set; the
    /// intersection is what's actually live for this connection.
    /// Defaults to `[]` for clients that need only the core methods.
    #[serde(default)]
    pub features: Vec<String>,
}

/// `system.handshake` response result.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct HandshakeResult {
    /// Protocol version the agent picked from
    /// [`HandshakeParams::protocol`].
    pub protocol: u32,
    /// Agent binary version. Drives the client's "Agent version
    /// mismatch — restart?" toast.
    pub agent_version: String,
    /// Features the agent itself supports. Per SPEC §2.12.6 this
    /// MAY be a superset of what the client asked for — the client
    /// is expected to enable only the intersection.
    pub features: Vec<String>,
    /// Session identity (SPEC §2.12.4) — agent reads this from the
    /// OS at connect time, not from the client payload, so the
    /// values here are authoritative.
    pub session: HandshakeSession,
}

/// Session info derived from the OS at connect time, surfaced back
/// to the client so the UI can label "logged in as
/// `DOMAIN\\alice`" without a second round-trip.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct HandshakeSession {
    /// `DOMAIN\\username` (Windows) or `username` (Linux/macOS).
    /// Derived from the OS token, not the payload.
    pub user: String,
    /// Console / RDP session id (Windows). On Linux/macOS this is
    /// the UID (which serves the same "which interactive shell is
    /// the caller in" role).
    pub session_id: u32,
    /// The PC's `pc_id` (matches the agent's `pc_id` everywhere
    /// else — `Heartbeat`, `ExecResult`, audit log, …). Carried in
    /// the handshake so the client's UI doesn't need a separate
    /// "which PC am I?" call.
    pub pc_id: String,
}

/// Well-known feature flag names. New flags are added here so the
/// agent + client + dispatcher all share one source of truth. SPEC
/// §2.12.6 says optional methods MUST be gated via these flags so
/// older clients/agents degrade gracefully.
pub mod features {
    pub const PUSH_NOTIFICATIONS: &str = "push.notifications";
    pub const PUSH_JOBS: &str = "push.jobs";
    pub const PUSH_STATE: &str = "push.state";
    pub const SUPPORT_DIAGNOSTICS: &str = "support.diagnostics";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_params_round_trips_through_json() {
        let p = HandshakeParams {
            client: "kanade-client".into(),
            client_version: "0.1.0".into(),
            protocol: vec![PROTOCOL_V1],
            features: vec![
                features::PUSH_NOTIFICATIONS.into(),
                features::PUSH_JOBS.into(),
                features::PUSH_STATE.into(),
            ],
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: HandshakeParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.client, "kanade-client");
        assert_eq!(back.client_version, "0.1.0");
        assert_eq!(back.protocol, vec![PROTOCOL_V1]);
        assert!(back.features.contains(&"push.notifications".to_string()));
    }

    #[test]
    fn handshake_params_accepts_missing_features() {
        // Older / lighter clients may omit `features` entirely. The
        // serde default keeps the decode green.
        let wire = r#"{"client":"kanade-client","client_version":"0.0.1","protocol":[1]}"#;
        let p: HandshakeParams = serde_json::from_str(wire).unwrap();
        assert!(p.features.is_empty());
    }

    #[test]
    fn handshake_result_spec_example_decodes() {
        // Verbatim from SPEC §2.12.6 — pinned so a struct rename
        // can't silently break the documented contract.
        let wire = r#"{
            "protocol": 1,
            "agent_version": "0.4.0",
            "features": ["push.notifications","push.jobs","push.state","support.diagnostics"],
            "session": {"user":"DOMAIN\\alice","session_id":2,"pc_id":"PC1234"}
        }"#;
        let r: HandshakeResult = serde_json::from_str(wire).expect("decode");
        assert_eq!(r.protocol, 1);
        assert_eq!(r.agent_version, "0.4.0");
        assert_eq!(r.features.len(), 4);
        assert_eq!(r.session.user, "DOMAIN\\alice");
        assert_eq!(r.session.session_id, 2);
        assert_eq!(r.session.pc_id, "PC1234");
    }

    #[test]
    fn handshake_protocol_negotiation_supports_multiple_versions() {
        // When v2 lands, clients will advertise `protocol:[1,2]`.
        // The struct must accept multi-element arrays today so the
        // upgrade path doesn't need a wire change.
        let wire = r#"{"client":"c","client_version":"0.0.1","protocol":[1,2,3]}"#;
        let p: HandshakeParams = serde_json::from_str(wire).unwrap();
        assert_eq!(p.protocol, vec![1, 2, 3]);
    }
}
