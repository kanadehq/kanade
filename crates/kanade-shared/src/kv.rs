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

#[cfg(test)]
mod tests {
    use super::*;

    /// NATS KV bucket names must be domain-safe ASCII (a-z, A-Z, 0-9, _, -).
    /// Lock the constants down so a future edit doesn't introduce a `.` and
    /// break create_key_value silently on the broker side.
    #[test]
    fn bucket_names_are_domain_safe() {
        for name in [
            BUCKET_SCRIPT_CURRENT,
            BUCKET_SCRIPT_STATUS,
            BUCKET_AGENTS_STATE,
            BUCKET_AGENT_CONFIG,
            BUCKET_SCHEDULES,
            OBJECT_AGENT_RELEASES,
        ] {
            assert!(
                !name.contains('.'),
                "bucket name {name:?} contains a dot, which NATS KV rejects"
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "bucket name {name:?} has non-domain-safe characters"
            );
        }
    }

    #[test]
    fn stream_names_are_unique() {
        let names = [
            STREAM_INVENTORY,
            STREAM_RESULTS,
            STREAM_DEPLOY,
            STREAM_EVENTS,
            STREAM_AUDIT,
        ];
        let mut deduped = names.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            names.len(),
            "stream constants collide: {names:?}"
        );
    }

    #[test]
    fn script_status_strings() {
        assert_eq!(SCRIPT_STATUS_ACTIVE, "ACTIVE");
        assert_eq!(SCRIPT_STATUS_REVOKED, "REVOKED");
        assert_ne!(SCRIPT_STATUS_ACTIVE, SCRIPT_STATUS_REVOKED);
    }

    #[test]
    fn key_agent_target_version_constant() {
        assert_eq!(KEY_AGENT_TARGET_VERSION, "target_version");
    }
}
