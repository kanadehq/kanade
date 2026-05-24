//! JSON-RPC 2.0 envelope types for KLP (SPEC §2.12.3).
//!
//! Three message shapes flow over the framed transport (Named Pipe
//! on Windows, Unix Domain Socket on Linux/macOS):
//!
//! - [`RpcRequest`] — `{jsonrpc, id, method, params}`. Carries an
//!   `id` so the recipient can correlate the matching response.
//! - [`RpcNotification`] — `{jsonrpc, method, params}`. No `id`,
//!   no response. Used for server push (`notifications.new`,
//!   `jobs.progress`, `state.changed`).
//! - [`RpcResponse`] — `{jsonrpc, id, result|error}`. Exactly one
//!   of `result` or `error` is present.
//!
//! [`RpcMessage`] is an untagged enum over the three for the read
//! side of the connection (one decoder, three possible shapes). The
//! write side picks the concrete type directly.
//!
//! `id` is modelled as a [`String`] to match SPEC §2.12.3's "UUID v7
//! 推奨" guidance — JSON-RPC 2.0 allows numbers and null too, but
//! KLP is a closed two-party protocol where both ends are ours, so
//! we narrow to the form we actually use. Inbound non-string ids
//! fail decode and the agent returns `InvalidRequest`.
//!
//! `params` and `result` are typed as [`serde_json::Value`] at the
//! envelope layer so the dispatcher can route on `method` BEFORE
//! committing to a payload schema. Each per-method module
//! (`handshake`, `system`, `jobs`, …) then `serde_json::from_value`s
//! into its strongly-typed params/result struct. This is a
//! deliberate trade — one extra (de)serialise hop in exchange for
//! the envelope staying method-agnostic, which is what makes the
//! dispatcher implementable as a `match method.as_str()` block.

use serde::{Deserialize, Serialize};

use super::error::RpcError;

/// The version string every KLP message carries in the `jsonrpc`
/// field. Pinned to `"2.0"` per the JSON-RPC spec; KLP doesn't
/// negotiate a different RPC version — protocol evolution happens
/// through the handshake's `protocol` field (SPEC §2.12.6).
pub const JSONRPC_VERSION: &str = "2.0";

/// Client → Agent request that expects a response (correlated by `id`).
///
/// SPEC shape:
/// ```jsonc
/// {"jsonrpc":"2.0","id":"01931a8e-...","method":"system.handshake",
///  "params":{...}}
/// ```
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    /// `params` is wire-optional — methods like `system.ping` take
    /// no arguments and SHOULD omit the field rather than send
    /// `null`. Decoders see `serde_json::Value::Null` for either
    /// form, so callers must not rely on absent-vs-null to carry
    /// meaning.
    #[serde(default, skip_serializing_if = "is_null")]
    pub params: serde_json::Value,
}

/// Server-push or fire-and-forget message with no response (no `id`).
///
/// Used for `notifications.new`, `jobs.progress`, `state.changed`
/// (Agent → Client) and, when needed, request-shaped Client → Agent
/// messages that don't want a response (none currently — kept here
/// for symmetry with JSON-RPC 2.0).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "is_null")]
    pub params: serde_json::Value,
}

/// Response to a [`RpcRequest`]. Exactly one of `result` or `error`
/// is populated — see [`RpcResponsePayload`].
///
/// Modelled as a struct with a flattened payload enum (rather than
/// two field options) so the type system enforces the spec's
/// "exactly one of" requirement: it's impossible to construct a
/// response that has both, or neither.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: String,
    #[serde(flatten)]
    pub payload: RpcResponsePayload,
}

/// Either-or payload for [`RpcResponse`]. `serde(untagged)` means
/// each variant is recognised purely by which key (`result` or
/// `error`) is present on the wire.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
#[serde(untagged)]
pub enum RpcResponsePayload {
    /// Success path. `result` may be any JSON value — including
    /// `null` for void methods like `notifications.unsubscribe`.
    Ok { result: serde_json::Value },
    /// Failure path. See [`RpcError`] for the error model.
    Err { error: RpcError },
}

/// Top-level decoded message for the agent's read loop. Inbound
/// bytes are parsed into this enum once; the dispatcher then
/// matches on the variant to route.
///
/// Untagged enum, decoded by trying variants in declaration order:
/// `Response` first (it owns `result`/`error`, neither of which
/// appear on requests), then `Request` (has both `id` and
/// `method`), then `Notification` (has `method` but no `id`). The
/// ordering matters — putting `Request` first would let it greedily
/// match `{id, method, error}` because `params` is optional and the
/// extra `error` field is silently ignored by serde-derived structs.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
#[serde(untagged)]
pub enum RpcMessage {
    Response(RpcResponse),
    Request(RpcRequest),
    Notification(RpcNotification),
}

impl RpcRequest {
    /// Build a typed request. Serialises `params` to JSON eagerly so
    /// later dispatch is cheap and the failure surface is just this
    /// call — no surprises mid-send.
    pub fn new<P: Serialize>(
        id: impl Into<String>,
        method: impl Into<String>,
        params: &P,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params: serde_json::to_value(params)?,
        })
    }
}

impl RpcNotification {
    /// Build a typed notification (no id, no response).
    pub fn new<P: Serialize>(
        method: impl Into<String>,
        params: &P,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: serde_json::to_value(params)?,
        })
    }
}

impl RpcResponse {
    /// Build a success response for a given request `id` from a
    /// typed result. `R = ()` is encoded as JSON `null`, matching
    /// SPEC §2.12.7's `{"result":null}` for void method returns.
    pub fn ok<R: Serialize>(id: impl Into<String>, result: &R) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            payload: RpcResponsePayload::Ok {
                result: serde_json::to_value(result)?,
            },
        })
    }

    /// Build an error response from a [`RpcError`].
    pub fn err(id: impl Into<String>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            payload: RpcResponsePayload::Err { error },
        }
    }
}

fn is_null(v: &serde_json::Value) -> bool {
    v.is_null()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::error::ErrorKind;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct DummyParams {
        foo: String,
        bar: u32,
    }

    #[test]
    fn request_round_trips_through_json() {
        let req = RpcRequest::new(
            "u1",
            "system.handshake",
            &DummyParams {
                foo: "hello".into(),
                bar: 7,
            },
        )
        .expect("encode");
        let json = serde_json::to_string(&req).unwrap();
        // Spot-check wire shape — `params` is nested, not flattened.
        assert!(json.contains("\"jsonrpc\":\"2.0\""), "wire: {json}");
        assert!(json.contains("\"method\":\"system.handshake\""));
        assert!(json.contains("\"id\":\"u1\""));
        let back: RpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "u1");
        assert_eq!(back.method, "system.handshake");
        let p: DummyParams = serde_json::from_value(back.params).unwrap();
        assert_eq!(p.foo, "hello");
        assert_eq!(p.bar, 7);
    }

    #[test]
    fn request_without_params_omits_field_on_wire() {
        // SPEC §2.12.6's `system.ping` has no params — the
        // serializer SHOULD drop the field rather than emit
        // `"params":null`, since strict JSON-RPC parsers reject the
        // latter for some methods.
        let req = RpcRequest {
            jsonrpc: JSONRPC_VERSION.into(),
            id: "ping-1".into(),
            method: "system.ping".into(),
            params: serde_json::Value::Null,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("params").is_none(), "wire: {v:?}");
    }

    #[test]
    fn notification_decodes_without_id() {
        // SPEC §2.12.7 push: `notifications.new` arrives with no id.
        let wire = r#"{"jsonrpc":"2.0","method":"notifications.new",
                       "params":{"id":"notif-9f3a"}}"#;
        let m: RpcMessage = serde_json::from_str(wire).unwrap();
        match m {
            RpcMessage::Notification(n) => {
                assert_eq!(n.method, "notifications.new");
                assert_eq!(n.params["id"], "notif-9f3a");
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[test]
    fn success_response_decodes_and_round_trips() {
        let r =
            RpcResponse::ok("u3", &serde_json::json!({"subscription":"sub-n-1"})).expect("encode");
        let json = serde_json::to_string(&r).unwrap();
        // Critical: `result` must appear on the wire, not nested in
        // a `payload` field — the flatten attribute does the work.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("result").is_some(), "wire: {v:?}");
        assert!(v.get("error").is_none());
        // And the message-level decoder must classify it as Response.
        let m: RpcMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(m, RpcMessage::Response(_)));
    }

    #[test]
    fn error_response_decodes_and_round_trips() {
        let r = RpcResponse::err(
            "u5",
            RpcError::new(
                ErrorKind::Unauthorized,
                "manifest 'reboot' has user_invokable=false",
            ),
        );
        let json = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("error").is_some(), "wire: {v:?}");
        assert!(v.get("result").is_none());
        assert_eq!(v["error"]["code"], -32000);

        // Round-trip preserves the discriminant.
        let back: RpcResponse = serde_json::from_str(&json).unwrap();
        match back.payload {
            RpcResponsePayload::Err { error } => assert_eq!(error.code, -32000),
            other => panic!("expected Err payload, got {other:?}"),
        }
    }

    #[test]
    fn message_decoder_distinguishes_request_from_response() {
        // The tricky case: a Request and a Response both carry `id`.
        // The decoder MUST recognise Response by the presence of
        // `result` (or `error`), not by id-vs-method, because there
        // are no required-method requests we send today that lack
        // params.
        let request_wire = r#"{"jsonrpc":"2.0","id":"u1","method":"system.ping"}"#;
        let response_wire = r#"{"jsonrpc":"2.0","id":"u1","result":null}"#;

        match serde_json::from_str::<RpcMessage>(request_wire).unwrap() {
            RpcMessage::Request(r) => assert_eq!(r.method, "system.ping"),
            other => panic!("expected Request, got {other:?}"),
        }
        match serde_json::from_str::<RpcMessage>(response_wire).unwrap() {
            RpcMessage::Response(r) => assert_eq!(r.id, "u1"),
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn void_result_serialises_as_null() {
        // SPEC §2.12.7's unsubscribe response is `{"result":null}`.
        let r = RpcResponse::ok("u4", &()).expect("encode");
        let v = serde_json::to_value(&r).unwrap();
        assert!(v["result"].is_null(), "wire: {v}");
    }
}
