//! KLP method dispatcher.
//!
//! Reads one [`kanade_shared::ipc::envelope::RpcMessage`] from the
//! connection, routes Request/Notification by `method` name, and
//! returns the [`kanade_shared::ipc::envelope::RpcResponse`] the
//! writer task should emit. Notifications get no response
//! (per JSON-RPC 2.0); they currently route to the same handlers
//! and the dispatcher discards the result.
//!
//! The pre-handshake gate (SPEC §2.12.6 — non-handshake methods
//! before handshake return `InvalidRequest`) lives here so every
//! handler stays handshake-agnostic and the gate has exactly one
//! place to live.
//!
//! Dispatcher errors fall into three buckets:
//!
//! - `Err(RpcError)` from a handler → wrapped into the response's
//!   `error` slot via [`RpcResponse::err`].
//! - `anyhow::Error` from a handler → mapped to
//!   [`ErrorKind::InternalError`] (-32603) with the original
//!   message in `data.detail`. Agent log carries the full
//!   backtrace.
//! - Envelope-level parse failures from the framing loop above
//!   never reach this function — they're handled by the
//!   per-connection task in `server.rs` (it sends
//!   `err_anonymous(ParseError)` and drops the connection).
//!
//! `dispatch_request` is async because some handlers do file I/O
//! (`system.log_tail`); pure-CPU handlers (handshake, ping,
//! version) just await trivially.

use kanade_shared::ipc::envelope::{RpcRequest, RpcResponse, decode_params};
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::handshake::HandshakeParams;
use kanade_shared::ipc::jobs::{JobsExecuteParams, JobsKillParams, JobsListParams};
use kanade_shared::ipc::method;
use kanade_shared::ipc::state::{
    StateSnapshotParams, StateSubscribeParams, StateUnsubscribeParams,
};
use kanade_shared::ipc::system::{LogTailParams, PingParams, VersionParams};
use tracing::warn;

use super::connection::ConnectionState;
use super::handlers;

/// Top-level dispatch for one [`RpcRequest`]. Always returns an
/// [`RpcResponse`] — even handler errors get wrapped into the
/// envelope (KLP is a closed two-party protocol; we never just
/// drop a request id without a reply).
pub async fn dispatch_request(conn: &mut ConnectionState, req: RpcRequest) -> RpcResponse {
    let result = dispatch_inner(conn, &req).await;
    match result {
        Ok(value) => RpcResponse {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: Some(req.id),
            payload: kanade_shared::ipc::envelope::RpcResponsePayload::Ok { result: value },
        },
        Err(err) => RpcResponse::err(req.id, err),
    }
}

/// Inner dispatch returning the typed-as-Value result OR an
/// `RpcError`. Separated so the [`RpcResponse`] envelope assembly
/// happens in exactly one place above.
async fn dispatch_inner(
    conn: &mut ConnectionState,
    req: &RpcRequest,
) -> std::result::Result<serde_json::Value, RpcError> {
    // Pre-handshake gate: only system.handshake is allowed
    // before the conn has agreed on a protocol version.
    if !conn.handshake_complete() && req.method != method::SYSTEM_HANDSHAKE {
        return Err(RpcError::new(
            ErrorKind::InvalidRequest,
            format!(
                "method '{}' requires handshake; call system.handshake first",
                req.method
            ),
        ));
    }

    match req.method.as_str() {
        method::SYSTEM_HANDSHAKE => {
            // HandshakeParams has required fields (client,
            // client_version, protocol) and so has no Default; route
            // directly through serde so missing-field errors surface
            // as InvalidParams. The `decode_params` helper is only
            // for methods whose params struct implements Default.
            let params: HandshakeParams =
                serde_json::from_value(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::system::handle_handshake(conn, params)?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::SYSTEM_PING => {
            let params: PingParams = decode_params(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::system::handle_ping(conn, params)?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::SYSTEM_VERSION => {
            let params: VersionParams =
                decode_params(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::system::handle_version(conn, params)?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::SYSTEM_LOG_TAIL => {
            let params: LogTailParams =
                decode_params(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::system::handle_log_tail(conn, params).await?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::STATE_SNAPSHOT => {
            let params: StateSnapshotParams =
                decode_params(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::state::handle_state_snapshot(conn, params)?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::STATE_SUBSCRIBE => {
            let params: StateSubscribeParams =
                decode_params(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::state::handle_state_subscribe(conn, params)?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::STATE_UNSUBSCRIBE => {
            // StateUnsubscribeParams has the required `subscription`
            // field — no Default, so route directly through serde
            // like the handshake path.
            let params: StateUnsubscribeParams =
                serde_json::from_value(req.params.clone()).map_err(invalid_params)?;
            handlers::state::handle_state_unsubscribe(conn, params)?;
            // SPEC §2.12.7's unsubscribe response is `{"result":null}`.
            Ok(serde_json::Value::Null)
        }
        method::JOBS_LIST => {
            let params: JobsListParams =
                decode_params(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::jobs::handle_jobs_list(conn, params).await?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::JOBS_EXECUTE => {
            // JobsExecuteParams.id is required (no Default) — route
            // through serde directly so a missing id surfaces as
            // InvalidParams, like the handshake path.
            let params: JobsExecuteParams =
                serde_json::from_value(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::jobs::handle_jobs_execute(conn, params).await?;
            serde_json::to_value(&result).map_err(internal)
        }
        method::JOBS_KILL => {
            let params: JobsKillParams =
                serde_json::from_value(req.params.clone()).map_err(invalid_params)?;
            let result = handlers::jobs::handle_jobs_kill(conn, params).await?;
            serde_json::to_value(&result).map_err(internal)
        }
        // Every other v1 method is reserved but not implemented
        // in this PR — answer MethodNotFound so client code can
        // tell "agent doesn't know about this" apart from
        // "agent crashed". Each addition in a follow-up PR
        // replaces one arm with a real handler.
        unknown => {
            warn!(
                method = %unknown,
                pc_id = %conn.pc_id,
                user = %conn.peer.user,
                "KLP method not yet implemented in this PR",
            );
            Err(RpcError::new(
                ErrorKind::MethodNotFound,
                format!("method '{unknown}' is not implemented in this agent build"),
            ))
        }
    }
}

fn invalid_params(e: serde_json::Error) -> RpcError {
    RpcError::new(ErrorKind::InvalidParams, e.to_string())
}

fn internal(e: serde_json::Error) -> RpcError {
    RpcError::new(ErrorKind::InternalError, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klp::auth::PeerCredentials;
    use kanade_shared::ipc::envelope::RpcResponsePayload;
    use kanade_shared::ipc::handshake::PROTOCOL_V1;
    use kanade_shared::ipc::state::StateSnapshot;
    use kanade_shared::wire::EffectiveConfig;
    use std::path::PathBuf;
    use tokio::sync::{mpsc, watch};

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

    fn fresh_conn() -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let (_state_tx, state_rx) = watch::channel(dummy_snapshot());
        let (push_tx, _push_rx) = mpsc::channel(8);
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.41.0".into(),
            cfg_rx,
            state_rx,
            PathBuf::from("agent.log"),
            push_tx,
        )
    }

    fn handshake_req() -> RpcRequest {
        RpcRequest::new(
            "h1",
            method::SYSTEM_HANDSHAKE,
            &HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.1.0".into(),
                protocol: vec![PROTOCOL_V1],
                features: vec![],
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pre_handshake_ping_returns_invalid_request() {
        let mut conn = fresh_conn();
        let req = RpcRequest {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: "p1".into(),
            method: method::SYSTEM_PING.to_string(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch_request(&mut conn, req).await;
        match resp.payload {
            RpcResponsePayload::Err { error } => {
                let data = error.data.expect("data populated");
                assert_eq!(data.kind, ErrorKind::InvalidRequest);
                assert!(data.detail.contains("requires handshake"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_then_ping_succeeds() {
        let mut conn = fresh_conn();

        let h_resp = dispatch_request(&mut conn, handshake_req()).await;
        assert!(
            matches!(h_resp.payload, RpcResponsePayload::Ok { .. }),
            "handshake should succeed: {:?}",
            h_resp.payload,
        );
        assert!(conn.handshake_complete());

        let p_req = RpcRequest {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: "p1".into(),
            method: method::SYSTEM_PING.to_string(),
            // Test the "params omitted = Null" path that decode_params
            // exists for.
            params: serde_json::Value::Null,
        };
        let p_resp = dispatch_request(&mut conn, p_req).await;
        match p_resp.payload {
            RpcResponsePayload::Ok { result } => {
                assert!(result.get("agent_time").is_some(), "wire: {result}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let mut conn = fresh_conn();
        // Handshake first so we get past the gate.
        let _ = dispatch_request(&mut conn, handshake_req()).await;

        let req = RpcRequest {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: "u1".into(),
            // A still-unimplemented method: jobs.list now routes to a
            // real handler, so pick one whose family hasn't landed yet
            // (support.* / maintenance.* / jobs.execute are next).
            method: "support.upload_diagnostics".to_string(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch_request(&mut conn, req).await;
        match resp.payload {
            RpcResponsePayload::Err { error } => {
                let data = error.data.expect("data populated");
                assert_eq!(data.kind, ErrorKind::MethodNotFound);
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_with_wrong_params_returns_invalid_params() {
        let mut conn = fresh_conn();
        // Send malformed params (array instead of object) — should
        // route to InvalidParams, not crash the dispatcher.
        let req = RpcRequest {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: "h1".into(),
            method: method::SYSTEM_HANDSHAKE.to_string(),
            params: serde_json::json!(["wrong", "shape"]),
        };
        let resp = dispatch_request(&mut conn, req).await;
        match resp.payload {
            RpcResponsePayload::Err { error } => {
                let data = error.data.expect("data populated");
                assert_eq!(data.kind, ErrorKind::InvalidParams);
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn response_id_correlates_to_request_id() {
        let mut conn = fresh_conn();
        let mut req = handshake_req();
        req.id = "correlation-test-123".into();
        let resp = dispatch_request(&mut conn, req).await;
        assert_eq!(resp.id.as_deref(), Some("correlation-test-123"));
    }
}
