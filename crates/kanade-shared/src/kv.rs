//! NATS KV bucket name + key helpers (spec §2.3.2).
//!
//! NATS KV bucket names must be domain-safe ASCII (a-z, A-Z, 0-9, _, -),
//! so the spec's dotted names (`script.current`, `script.status`) are
//! flattened to underscore form here.

pub const BUCKET_SCRIPT_CURRENT: &str = "script_current";
pub const BUCKET_SCRIPT_STATUS: &str = "script_status";
pub const BUCKET_AGENTS_STATE: &str = "agents_state";
pub const BUCKET_AGENT_CONFIG: &str = "agent_config";
pub const BUCKET_SCHEDULES: &str = "schedules";

/// Object Store bucket holding raw agent binaries (one object per
/// version, e.g. `0.2.0` → file bytes).
pub const OBJECT_AGENT_RELEASES: &str = "agent_releases";

/// Key inside [`BUCKET_AGENT_CONFIG`] carrying the broadcast target
/// version. Agents watch this key and self-update when their running
/// version drifts.
pub const KEY_AGENT_TARGET_VERSION: &str = "target_version";

pub const SCRIPT_STATUS_ACTIVE: &str = "ACTIVE";
pub const SCRIPT_STATUS_REVOKED: &str = "REVOKED";

pub const STREAM_INVENTORY: &str = "INVENTORY";
pub const STREAM_RESULTS: &str = "RESULTS";
pub const STREAM_DEPLOY: &str = "DEPLOY";
pub const STREAM_EVENTS: &str = "EVENTS";
pub const STREAM_AUDIT: &str = "AUDIT";
