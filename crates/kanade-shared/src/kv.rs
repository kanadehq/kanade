//! NATS KV bucket name + key helpers (spec §2.3.2).
//!
//! NATS KV bucket names must be domain-safe ASCII (a-z, A-Z, 0-9, _, -),
//! so the spec's dotted names (`script.current`, `script.status`) are
//! flattened to underscore form here.

pub const BUCKET_SCRIPT_CURRENT: &str = "script_current";
pub const BUCKET_SCRIPT_STATUS: &str = "script_status";
pub const BUCKET_AGENTS_STATE: &str = "agents_state";
pub const BUCKET_AGENT_CONFIG: &str = "agent_config";
pub const BUCKET_AGENT_GROUPS: &str = "agent_groups";
pub const BUCKET_SCHEDULES: &str = "schedules";

/// Job catalog (v0.15) — operator-registered Manifests, keyed by
/// `manifest.id`. Schedules and ad-hoc `kanade run --job-id ...` look
/// jobs up here; the wire never round-trips an inline Manifest body
/// through a Schedule again. Editing a job in-place retroactively
/// changes what future schedule fires deploy.
pub const BUCKET_JOBS: &str = "jobs";

/// Object Store bucket holding raw agent binaries (one object per
/// version, e.g. `0.2.0` → file bytes).
pub const OBJECT_AGENT_RELEASES: &str = "agent_releases";

/// Key inside [`BUCKET_AGENT_CONFIG`] carrying the broadcast target
/// version. Agents watch this key and self-update when their running
/// version drifts.
pub const KEY_AGENT_TARGET_VERSION: &str = "target_version";

/// Sprint 6 layered-config keys inside [`BUCKET_AGENT_CONFIG`]:
///   * `global`        — fleet-wide default ConfigScope JSON
///   * `groups.<name>` — per-group override (partial ConfigScope)
///   * `pcs.<pc_id>`   — per-pc override (partial ConfigScope)
///
/// The `groups.` / `pcs.` prefixes let a `kv.keys()` walk pick out
/// just the rows in one scope when listing.
pub const KEY_AGENT_CONFIG_GLOBAL: &str = "global";
pub const PREFIX_AGENT_CONFIG_GROUPS: &str = "groups.";
pub const PREFIX_AGENT_CONFIG_PCS: &str = "pcs.";

pub fn agent_config_group_key(group: &str) -> String {
    format!("{PREFIX_AGENT_CONFIG_GROUPS}{group}")
}

pub fn agent_config_pc_key(pc_id: &str) -> String {
    format!("{PREFIX_AGENT_CONFIG_PCS}{pc_id}")
}

/// Inverse of [`agent_config_group_key`] — returns the bare group
/// name if `key` carries the groups-scope prefix, else `None`.
pub fn parse_agent_config_group_key(key: &str) -> Option<&str> {
    key.strip_prefix(PREFIX_AGENT_CONFIG_GROUPS)
}

/// Inverse of [`agent_config_pc_key`].
pub fn parse_agent_config_pc_key(key: &str) -> Option<&str> {
    key.strip_prefix(PREFIX_AGENT_CONFIG_PCS)
}

pub const SCRIPT_STATUS_ACTIVE: &str = "ACTIVE";
pub const SCRIPT_STATUS_REVOKED: &str = "REVOKED";

pub const STREAM_INVENTORY: &str = "INVENTORY";
pub const STREAM_RESULTS: &str = "RESULTS";
pub const STREAM_EXEC: &str = "EXEC";
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
            BUCKET_AGENT_GROUPS,
            BUCKET_SCHEDULES,
            BUCKET_JOBS,
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
            STREAM_EXEC,
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

    #[test]
    fn agent_config_group_key_round_trips() {
        let k = agent_config_group_key("canary");
        assert_eq!(k, "groups.canary");
        assert_eq!(parse_agent_config_group_key(&k), Some("canary"));
    }

    #[test]
    fn agent_config_pc_key_round_trips() {
        let k = agent_config_pc_key("MINIPC-01");
        assert_eq!(k, "pcs.MINIPC-01");
        assert_eq!(parse_agent_config_pc_key(&k), Some("MINIPC-01"));
    }

    #[test]
    fn agent_config_scope_keys_do_not_collide() {
        // Belt + braces: make sure no pc id starting with "groups." would
        // be misparsed (or vice versa). The prefixes are distinct because
        // they each end in `.` and the parent buckets disagree on what
        // comes after — pcs holds host names, groups holds membership
        // names — but locking the invariant in a test stops a future
        // rename from breaking it.
        assert_ne!(PREFIX_AGENT_CONFIG_GROUPS, PREFIX_AGENT_CONFIG_PCS);
        assert!(parse_agent_config_group_key("pcs.someone").is_none());
        assert!(parse_agent_config_pc_key("groups.someone").is_none());
        assert_eq!(parse_agent_config_group_key("global"), None);
        assert_eq!(parse_agent_config_pc_key("global"), None);
    }
}
