//! `system.*` (non-handshake) method types — `system.ping`,
//! `system.version`, `system.log_tail`.
//!
//! `system.handshake` lives in [`super::handshake`] because it has
//! enough surface area (params, result, session, features) to
//! deserve its own module.

use serde::{Deserialize, Serialize};

// ---------- system.ping ----------

/// `system.ping` takes no params and returns no body. Both shapes
/// are kept as explicit unit-like structs (rather than `()`) so the
/// dispatcher can write `from_value::<PingParams>(_)` symmetrically
/// with every other method.
///
/// Wire form: `{}` (empty object). Decoders accept absent params
/// too thanks to the envelope's `serde(default)` on `params`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct PingParams {}

/// `system.ping` response. Carries the agent's monotonic clock at
/// the moment it answered — clients use the (sent_at, received_at)
/// pair for one-way latency estimates without needing a separate
/// API.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct PingResult {
    /// Agent-side wall-clock time when the ping was answered, UTC.
    /// Round-trip variance comes from queueing + scheduling, not
    /// clock skew — this is wall-clock for log correlation, not for
    /// monotonic measurement.
    pub agent_time: chrono::DateTime<chrono::Utc>,
}

// ---------- system.version ----------

/// `system.version` takes no params.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct VersionParams {}

/// `system.version` response — agent + client app version pair (the
/// client may not know its own published "intended" version when
/// auto-update is in flight, hence why both come from the agent
/// which owns the manifest).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct VersionResult {
    /// Current agent binary version (`CARGO_PKG_VERSION` of the
    /// running agent).
    pub agent_version: String,
    /// Version the agent self-updater is currently targeting. Equal
    /// to `agent_version` in steady state; differs while an update
    /// is downloading or pending restart. Lets the client surface a
    /// "restart pending" banner without scraping logs.
    pub target_agent_version: String,
    /// Version the client SHOULD be running (published by the
    /// backend through `agent_config`). The client compares this to
    /// its own `CARGO_PKG_VERSION` and prompts the user to relaunch
    /// when they differ. `None` until the backend has published a
    /// pinned client version (Sprint 8 deferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_client_version: Option<String>,
}

// ---------- system.log_tail ----------

/// `system.log_tail` params — sized "last N lines of agent.log" so
/// support handoff diagnostics fit a single message inside the 1 MiB
/// framing cap (SPEC §2.12.2).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct LogTailParams {
    /// How many trailing lines to return. Agent clamps to its
    /// internal cap (currently 1000) to keep responses bounded —
    /// callers asking for more will see truncation, not an error.
    /// Defaults to 200 when the field is absent.
    #[serde(default = "default_log_tail_lines")]
    pub lines: u32,
}

impl Default for LogTailParams {
    fn default() -> Self {
        Self {
            lines: default_log_tail_lines(),
        }
    }
}

fn default_log_tail_lines() -> u32 {
    200
}

/// `system.log_tail` response.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct LogTailResult {
    /// Raw log lines, newest last. Each entry is one line without
    /// its trailing newline. Agent's logger uses the standard
    /// `tracing-subscriber` format, so SPA support flows can
    /// concatenate with `"\n"` for display.
    pub lines: Vec<String>,
    /// `true` when the caller's request was clamped down to the
    /// agent's per-call cap (the agent currently caps at 1000
    /// lines per call to keep the response inside the 1 MiB
    /// framing cap from SPEC §2.12.2). Surfaced so support
    /// tooling can warn the user to also pull the file from disk
    /// via `support.upload_diagnostics`.
    ///
    /// NOT set merely because the on-disk file has more lines
    /// than what the caller asked for: if you asked for 200 and
    /// got 200, `truncated` is `false` even when the file holds
    /// 50 000 — the agent gave you exactly what you requested.
    #[serde(default)]
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ping_params_decodes_from_empty_object() {
        let _: PingParams = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn ping_result_round_trips_through_json() {
        let t = chrono::Utc.with_ymd_and_hms(2026, 5, 24, 0, 0, 0).unwrap();
        let r = PingResult { agent_time: t };
        let json = serde_json::to_string(&r).unwrap();
        let back: PingResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_time, t);
    }

    #[test]
    fn version_result_target_client_optional() {
        // Sprint 8 may ship without a pinned client version; the
        // field must round-trip cleanly when absent.
        let wire = r#"{"agent_version":"0.4.0","target_agent_version":"0.4.0"}"#;
        let r: VersionResult = serde_json::from_str(wire).unwrap();
        assert_eq!(r.agent_version, "0.4.0");
        assert!(r.target_client_version.is_none());
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("target_client_version"));
    }

    #[test]
    fn log_tail_params_defaults_to_200_lines() {
        let p = LogTailParams::default();
        assert_eq!(p.lines, 200);
        // Wire decode of an empty object also gets the default.
        let p: LogTailParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.lines, 200);
    }

    #[test]
    fn log_tail_result_truncated_defaults_to_false() {
        let wire = r#"{"lines":["a","b","c"]}"#;
        let r: LogTailResult = serde_json::from_str(wire).unwrap();
        assert_eq!(r.lines.len(), 3);
        assert!(!r.truncated);
    }
}
