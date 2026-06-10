//! KLP error model (SPEC §2.12.9).
//!
//! KLP carries errors inside the JSON-RPC 2.0 `error` object, with
//! the canonical envelope:
//!
//! ```jsonc
//! {"jsonrpc":"2.0","id":"u5","error":{
//!   "code": -32000,
//!   "message": "Job not user-invokable",
//!   "data": {"kind":"Unauthorized","detail":"manifest 'reboot' has user_invokable=false"}
//! }}
//! ```
//!
//! The `code` field follows JSON-RPC convention (-32700 / -32600
//! /-32601 / -32602 / -32603 are reserved by the spec; -32000 ..
//! -32099 are application-defined). `data.kind` is the canonical
//! machine-readable label — agents and clients should switch on
//! `kind`, not `code` (a future SPEC bump may reshuffle codes).

use serde::{Deserialize, Serialize};

/// All KLP-defined error kinds (SPEC §2.12.9 table).
///
/// Wire-encoded verbatim as the variant name (`"Unauthorized"`,
/// `"RateLimit"`, `"StaleProtocol"`, …) to match the SPEC §2.12.9
/// table 1:1 — this is the one place in the codebase that breaks
/// from the otherwise-uniform `snake_case` convention, because the
/// spec doc shows PascalCase wire and we keep that contract.
///
/// `#[non_exhaustive]` so SPEC §2.12.9 can grow new error kinds in
/// a future revision without forcing a wire-protocol bump —
/// downstream Rust consumers see a compile-time nudge to add a
/// wildcard arm.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// `-32700` — request body wasn't valid JSON.
    ParseError,
    /// `-32600` — JSON-RPC envelope rejected (missing `jsonrpc`
    /// field, non-string method, etc.) OR a non-handshake method
    /// was invoked before `system.handshake` completed.
    InvalidRequest,
    /// `-32601` — method name not registered on this agent's
    /// dispatcher.
    MethodNotFound,
    /// `-32602` — `params` failed schema validation for the named
    /// method.
    InvalidParams,
    /// `-32603` — agent-side panic / unexpected failure. Always
    /// returned with a redacted message — the original error lives
    /// in the agent log.
    InternalError,
    /// `-32000` — authorization failure. The connection is authed
    /// (we know the SID/UID) but the caller isn't allowed to do
    /// this thing: invoking a job whose manifest has
    /// `user_invokable=false`, killing a `run_id` belonging to
    /// another connection, ack-ing a notification not addressed to
    /// the caller's PC / group / `all` audience.
    Unauthorized,
    /// `-32001` — referenced `job_id` / `run_id` / `notification_id`
    /// doesn't exist.
    NotFound,
    /// `-32002` — agent isn't connected to NATS right now, so
    /// fan-out / publish-side operations can't be served. The
    /// client SHOULD show a transient banner and retry on the
    /// next state push.
    AgentDisconnected,
    /// `-32003` — connection exceeded the 60 req/min cap. The
    /// client SHOULD back off (the agent doesn't tell it for how
    /// long; 1 s is a safe minimum).
    RateLimit,
    /// `-32004` — handshake negotiated a protocol version that one
    /// side no longer supports. Treat as fatal: the client must
    /// upgrade (or the agent must be downgraded).
    StaleProtocol,
    /// `-32005` — message body exceeded the 1 MiB framing limit
    /// (SPEC §2.12.2). `stdout_chunk` payloads must be split before
    /// hitting this.
    PayloadTooLarge,
    /// #492: serde-level forward-compat catch-all — a newer peer's
    /// new error kind decodes here instead of making the older side
    /// fail to decode the whole RpcError. Maps to the JSON-RPC
    /// generic server-error code.
    #[serde(other)]
    Unknown,
}

impl ErrorKind {
    /// JSON-RPC `code` field for this kind. Pre-baked so the dispatch
    /// path doesn't accidentally drift away from the table — the only
    /// blessed mapping lives here.
    pub fn code(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::Unauthorized => -32000,
            Self::NotFound => -32001,
            Self::AgentDisconnected => -32002,
            Self::RateLimit => -32003,
            Self::StaleProtocol => -32004,
            Self::PayloadTooLarge => -32005,
            // JSON-RPC reserves -32099..-32000 for server errors;
            // -32099 marks "kind unknown to this build".
            Self::Unknown => -32099,
        }
    }

    /// Default human-readable `message`. Agents may override per-call
    /// when they have a more specific phrasing; tests and the spec
    /// table use these.
    pub fn default_message(self) -> &'static str {
        match self {
            Self::ParseError => "Parse error",
            Self::InvalidRequest => "Invalid Request",
            Self::MethodNotFound => "Method not found",
            Self::InvalidParams => "Invalid params",
            Self::InternalError => "Internal error",
            Self::Unauthorized => "Unauthorized",
            Self::NotFound => "Not found",
            Self::AgentDisconnected => "Agent disconnected from broker",
            Self::RateLimit => "Rate limit exceeded",
            Self::StaleProtocol => "Stale protocol version",
            Self::PayloadTooLarge => "Payload too large",
            Self::Unknown => "Unknown error kind (newer peer)",
        }
    }
}

/// JSON-RPC 2.0 `error` object (SPEC §2.12.9). Always paired with a
/// non-null `id` inside an [`super::envelope::RpcResponse`] — there
/// is no notion of an error notification in KLP.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    /// Structured detail. Wire-optional so the JSON-RPC reserved
    /// codes (-32600 / -32601 / -32602 / -32603 / -32700) can be
    /// returned with just `{code, message}` when the agent has no
    /// extra context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<RpcErrorData>,
}

/// Application-side payload inside [`RpcError::data`].
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct RpcErrorData {
    pub kind: ErrorKind,
    /// Free-form human-readable elaboration. Safe to surface in the
    /// SPA toast; never carries secrets (agent code redacts before
    /// constructing).
    pub detail: String,
}

impl RpcError {
    /// Build an error matching SPEC §2.12.9's canonical shape:
    /// `code` derived from `kind`, `message` defaulted from `kind`,
    /// `data` populated with `{kind, detail}`.
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            code: kind.code(),
            message: kind.default_message().to_string(),
            data: Some(RpcErrorData {
                kind,
                detail: detail.into(),
            }),
        }
    }

    /// Bare error without `data`. Use only for the JSON-RPC reserved
    /// codes where the spec allows omitting structured detail (parse
    /// errors before the envelope can be decoded, etc.).
    pub fn bare(kind: ErrorKind) -> Self {
        Self {
            code: kind.code(),
            message: kind.default_message().to_string(),
            data: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_codes_match_spec_table() {
        // SPEC §2.12.9 — pinned so a refactor of the table can't
        // silently drift codes apart.
        assert_eq!(ErrorKind::ParseError.code(), -32700);
        assert_eq!(ErrorKind::InvalidRequest.code(), -32600);
        assert_eq!(ErrorKind::MethodNotFound.code(), -32601);
        assert_eq!(ErrorKind::InvalidParams.code(), -32602);
        assert_eq!(ErrorKind::InternalError.code(), -32603);
        assert_eq!(ErrorKind::Unauthorized.code(), -32000);
        assert_eq!(ErrorKind::NotFound.code(), -32001);
        assert_eq!(ErrorKind::AgentDisconnected.code(), -32002);
        assert_eq!(ErrorKind::RateLimit.code(), -32003);
        assert_eq!(ErrorKind::StaleProtocol.code(), -32004);
        assert_eq!(ErrorKind::PayloadTooLarge.code(), -32005);
    }

    #[test]
    fn error_kind_serialises_pascal_case_per_spec() {
        // SPEC §2.12.9 wire form is the Rust variant name verbatim.
        let json = serde_json::to_string(&ErrorKind::StaleProtocol).unwrap();
        assert_eq!(json, "\"StaleProtocol\"");
        let json = serde_json::to_string(&ErrorKind::RateLimit).unwrap();
        assert_eq!(json, "\"RateLimit\"");
        let json = serde_json::to_string(&ErrorKind::Unauthorized).unwrap();
        assert_eq!(json, "\"Unauthorized\"");
    }

    #[test]
    fn rpc_error_new_round_trips_through_json() {
        let e = RpcError::new(
            ErrorKind::Unauthorized,
            "manifest 'reboot' has user_invokable=false",
        );
        let json = serde_json::to_string(&e).unwrap();
        let back: RpcError = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code, -32000);
        assert_eq!(back.message, "Unauthorized");
        let data = back.data.expect("data populated");
        assert_eq!(data.kind, ErrorKind::Unauthorized);
        assert_eq!(data.detail, "manifest 'reboot' has user_invokable=false");
    }

    #[test]
    fn rpc_error_bare_round_trips_without_data_field() {
        let e = RpcError::bare(ErrorKind::ParseError);
        let v = serde_json::to_value(&e).unwrap();
        // `data` SHOULD be absent on the wire (not `null`) so the
        // envelope matches strict JSON-RPC parsers.
        assert!(
            v.get("data").is_none(),
            "data field must be absent on the wire, got {v:?}",
        );
        assert_eq!(v["code"], -32700);
    }

    #[test]
    fn rpc_error_spec_example_decodes() {
        // Exact payload from SPEC §2.12.9. Pinned so a careless rename
        // of `kind` / `detail` breaks the test loudly.
        let wire = r#"{
          "code": -32000,
          "message": "Job not user-invokable",
          "data": {"kind":"Unauthorized","detail":"manifest 'reboot' has user_invokable=false"}
        }"#;
        let e: RpcError = serde_json::from_str(wire).expect("decode");
        assert_eq!(e.code, -32000);
        let data = e.data.expect("data populated");
        assert_eq!(data.kind, ErrorKind::Unauthorized);
        assert!(data.detail.contains("user_invokable=false"));
    }
}
