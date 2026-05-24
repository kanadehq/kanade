//! `system.*` method handlers (SPEC §2.12.5).
//!
//! This PR ships only the two methods needed to prove the
//! transport works end-to-end:
//!
//! - `system.handshake` — protocol-version negotiation + session
//!   info return (SPEC §2.12.6).
//! - `system.ping` — round-trip liveness check.
//!
//! `system.version` and `system.log_tail` ship in a follow-up PR
//! together with the state/notifications/jobs/support/maintenance
//! handlers — they don't add new wire surface, but they need
//! agent.log file plumbing that isn't part of the foundation.

use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::handshake::{HandshakeParams, HandshakeResult, PROTOCOL_V1};
use kanade_shared::ipc::system::{PingParams, PingResult};

use super::super::connection::ConnectionState;

/// Result type for KLP handlers — either a JSON-encoded response
/// payload or a [`RpcError`] the dispatcher will wrap into the
/// envelope's `error` slot. `anyhow::Error` is reserved for
/// internal failures the dispatcher turns into
/// [`ErrorKind::InternalError`] (-32603).
pub type HandlerResult<T> = std::result::Result<T, RpcError>;

/// Features this agent currently advertises in handshake. The
/// listener-foundation PR has the transport + handshake done but
/// no push handlers yet, so we ONLY advertise features whose
/// methods this PR actually routes — `push.notifications`,
/// `push.jobs`, `push.state`, and `support.diagnostics` are added
/// when their handlers land.
const SUPPORTED_FEATURES: &[&str] = &[];

/// `system.handshake` — protocol negotiation + session info.
///
/// SPEC §2.12.6:
/// - Pick the highest mutually-supported version from
///   `params.protocol`. KLP v1 only knows `PROTOCOL_V1` today.
/// - Return the agent's session info derived from the OS
///   (`conn.session()`), never from payload.
/// - If no protocol overlap, return
///   [`ErrorKind::StaleProtocol`].
pub fn handle_handshake(
    conn: &mut ConnectionState,
    params: HandshakeParams,
) -> HandlerResult<HandshakeResult> {
    if params.protocol.is_empty() {
        return Err(RpcError::new(
            ErrorKind::InvalidParams,
            "handshake.protocol must contain at least one version",
        ));
    }

    // Pick highest mutually-supported. Today we only know v1, so
    // the question collapses to "does the client mention v1?".
    let agreed = params
        .protocol
        .iter()
        .copied()
        .filter(|&v| v == PROTOCOL_V1)
        .max();

    let Some(agreed) = agreed else {
        return Err(RpcError::new(
            ErrorKind::StaleProtocol,
            format!(
                "no overlap with client versions {:?} (agent supports {:?})",
                params.protocol,
                [PROTOCOL_V1],
            ),
        ));
    };

    conn.mark_handshake(agreed);

    Ok(HandshakeResult {
        protocol: agreed,
        agent_version: conn.agent_version.clone(),
        features: SUPPORTED_FEATURES.iter().map(|&s| s.to_string()).collect(),
        session: conn.session(),
    })
}

/// `system.ping` — agent wall-clock at the moment it answered.
/// No params required; the envelope's `params: omitted` is
/// equivalent to `params: {}` via the
/// [`kanade_shared::ipc::envelope::decode_params`] helper.
pub fn handle_ping(_conn: &ConnectionState, _params: PingParams) -> HandlerResult<PingResult> {
    Ok(PingResult {
        agent_time: chrono::Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klp::auth::PeerCredentials;

    fn fresh_conn() -> ConnectionState {
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.40.0".into(),
        )
    }

    #[test]
    fn handshake_v1_only_client_succeeds() {
        let mut conn = fresh_conn();
        let result = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.1.0".into(),
                protocol: vec![PROTOCOL_V1],
                features: vec![],
            },
        )
        .expect("handshake should succeed");
        assert_eq!(result.protocol, PROTOCOL_V1);
        assert_eq!(result.agent_version, "0.40.0");
        assert_eq!(result.session.user, "DOMAIN\\alice");
        assert_eq!(result.session.pc_id, "PC1234");
        assert!(conn.handshake_complete());
    }

    #[test]
    fn handshake_picks_highest_mutual_version_from_multi_version_client() {
        // Future-proof: a client advertising [1, 2] talking to a
        // v1-only agent must downshift to 1, not error.
        let mut conn = fresh_conn();
        let result = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.2.0".into(),
                protocol: vec![1, 2, 3],
                features: vec![],
            },
        )
        .expect("handshake should succeed via downshift");
        assert_eq!(result.protocol, PROTOCOL_V1);
    }

    #[test]
    fn handshake_rejects_empty_protocol_list() {
        let mut conn = fresh_conn();
        let err = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.1.0".into(),
                protocol: vec![],
                features: vec![],
            },
        )
        .expect_err("empty protocol must fail");
        let data = err.data.as_ref().expect("data populated");
        assert_eq!(data.kind, ErrorKind::InvalidParams);
        assert!(!conn.handshake_complete(), "conn must stay pre-handshake");
    }

    #[test]
    fn handshake_rejects_when_no_version_overlap() {
        // Client speaks only v2 / v3; agent speaks only v1 → no
        // overlap → StaleProtocol per SPEC §2.12.6.
        let mut conn = fresh_conn();
        let err = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "9.9.9".into(),
                protocol: vec![2, 3],
                features: vec![],
            },
        )
        .expect_err("must fail with StaleProtocol");
        let data = err.data.as_ref().expect("data populated");
        assert_eq!(data.kind, ErrorKind::StaleProtocol);
        assert!(!conn.handshake_complete());
    }

    #[test]
    fn ping_returns_recent_agent_time() {
        let conn = fresh_conn();
        let before = chrono::Utc::now();
        let result = handle_ping(&conn, PingParams::default()).unwrap();
        let after = chrono::Utc::now();
        assert!(
            result.agent_time >= before && result.agent_time <= after,
            "agent_time {} should be in [{before}, {after}]",
            result.agent_time
        );
    }

    #[test]
    fn handshake_advertises_no_features_in_foundation_pr() {
        // Foundation PR has the transport + handshake done but no
        // push handlers yet — features SHOULD be empty until each
        // method actually lands (otherwise we'd lie to clients
        // about what we can do).
        let mut conn = fresh_conn();
        let result = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.1.0".into(),
                protocol: vec![PROTOCOL_V1],
                features: vec!["push.notifications".into()],
            },
        )
        .unwrap();
        assert!(
            result.features.is_empty(),
            "foundation PR ships no optional features yet; got {:?}",
            result.features,
        );
    }
}
