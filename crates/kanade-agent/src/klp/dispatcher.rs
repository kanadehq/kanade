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

use kanade_shared::ipc::envelope::{RpcRequest, RpcResponse, decode_params};
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::handshake::HandshakeParams;
use kanade_shared::ipc::method;
use kanade_shared::ipc::system::PingParams;
use tracing::warn;

use super::connection::ConnectionState;
use super::handlers;

/// Top-level dispatch for one [`RpcRequest`]. Always returns an
/// [`RpcResponse`] — even handler errors get wrapped into the
/// envelope (KLP is a closed two-party protocol; we never just
/// drop a request id without a reply).
pub fn dispatch_request(conn: &mut ConnectionState, req: RpcRequest) -> RpcResponse {
    let result = dispatch_inner(conn, &req);
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
fn dispatch_inner(
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

    #[test]
    fn pre_handshake_ping_returns_invalid_request() {
        let mut conn = fresh_conn();
        let req = RpcRequest {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: "p1".into(),
            method: method::SYSTEM_PING.to_string(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch_request(&mut conn, req);
        match resp.payload {
            RpcResponsePayload::Err { error } => {
                let data = error.data.expect("data populated");
                assert_eq!(data.kind, ErrorKind::InvalidRequest);
                assert!(data.detail.contains("requires handshake"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn handshake_then_ping_succeeds() {
        let mut conn = fresh_conn();

        let h_resp = dispatch_request(&mut conn, handshake_req());
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
        let p_resp = dispatch_request(&mut conn, p_req);
        match p_resp.payload {
            RpcResponsePayload::Ok { result } => {
                assert!(result.get("agent_time").is_some(), "wire: {result}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let mut conn = fresh_conn();
        // Handshake first so we get past the gate.
        let _ = dispatch_request(&mut conn, handshake_req());

        let req = RpcRequest {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: "u1".into(),
            method: "jobs.list".to_string(),
            params: serde_json::Value::Null,
        };
        let resp = dispatch_request(&mut conn, req);
        match resp.payload {
            RpcResponsePayload::Err { error } => {
                let data = error.data.expect("data populated");
                assert_eq!(data.kind, ErrorKind::MethodNotFound);
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn handshake_with_wrong_params_returns_invalid_params() {
        let mut conn = fresh_conn();
        // Send malformed params (array instead of object) — should
        // route to InvalidParams, not crash the dispatcher.
        let req = RpcRequest {
            jsonrpc: kanade_shared::ipc::envelope::JSONRPC_VERSION.to_string(),
            id: "h1".into(),
            method: method::SYSTEM_HANDSHAKE.to_string(),
            params: serde_json::json!(["wrong", "shape"]),
        };
        let resp = dispatch_request(&mut conn, req);
        match resp.payload {
            RpcResponsePayload::Err { error } => {
                let data = error.data.expect("data populated");
                assert_eq!(data.kind, ErrorKind::InvalidParams);
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn response_id_correlates_to_request_id() {
        let mut conn = fresh_conn();
        let mut req = handshake_req();
        req.id = "correlation-test-123".into();
        let resp = dispatch_request(&mut conn, req);
        assert_eq!(resp.id.as_deref(), Some("correlation-test-123"));
    }
}
