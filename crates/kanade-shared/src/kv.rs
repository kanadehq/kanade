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

pub const SCRIPT_STATUS_ACTIVE: &str = "ACTIVE";
pub const SCRIPT_STATUS_REVOKED: &str = "REVOKED";

pub const STREAM_INVENTORY: &str = "INVENTORY";
pub const STREAM_RESULTS: &str = "RESULTS";
pub const STREAM_DEPLOY: &str = "DEPLOY";
pub const STREAM_EVENTS: &str = "EVENTS";
pub const STREAM_AUDIT: &str = "AUDIT";
