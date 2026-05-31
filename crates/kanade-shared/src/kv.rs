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

/// Parallel "operator source-of-truth YAML" stores keyed identically
/// to `BUCKET_JOBS` / `BUCKET_SCHEDULES`. The agent / scheduler /
/// projector all keep reading the JSON KVs above — these buckets
/// exist only so the SPA's YAML editor can round-trip operator
/// comments + script indentation + block-scalar style exactly.
///
/// Population is opportunistic: any `POST` with a
/// `Content-Type: application/yaml` body stores the raw bytes here
/// alongside the parsed JSON; JSON-content-type POSTs fall back to a
/// `serde_yaml` dump so the buckets stay in lockstep with the JSON
/// store (operator just loses comments on that path).
pub const BUCKET_JOBS_YAML: &str = "jobs_yaml";
pub const BUCKET_SCHEDULES_YAML: &str = "schedules_yaml";

/// Object Store bucket holding raw agent binaries (one object per
/// version, e.g. `0.2.0` → file bytes).
pub const OBJECT_AGENT_RELEASES: &str = "agent_releases";

/// Object Store holding **generic application packages** — anything
/// the agent / kitting scripts pull down + install on endpoints.
/// First consumer is the kanade-client app, but the bucket is
/// intentionally generic: third-party installers (Webex, Teams,
/// custom MSI bundles), upgrade scripts, configuration archives,
/// etc. all live here.
///
/// Object keys are `<name>/<version>` — operator picks `<name>`
/// once per package family (e.g. `kanade-client`,
/// `webex-meetings`), then `<version>` per release (e.g.
/// `0.41.0`, `2025.03`). Slashes are explicitly allowed by NATS
/// Object Store key rules; the SPA / CLI / HTTP routes all carry
/// the pair as two path segments.
///
/// Why a separate bucket from `agent_releases`:
/// - `agent_releases` is fleet-critical (the agent's own self-
///   update path). Keeping it small + audited matters.
/// - `app_packages` is operator-curated user-space content. The
///   lifecycle is different (operators add/remove packages
///   freely; agent releases follow the release.yml pipeline).
pub const OBJECT_APP_PACKAGES: &str = "app_packages";

/// Object Store holding **manifest script bodies** referenced by
/// `Execute::script_object` (SPEC §2.4.1's alternative to inline
/// `script:` / repo-local `script_file:`). Per yukimemi/kanade
/// issue #210, this is the "Plan B 4-bucket layout" sibling of
/// `app_packages` — separated because scripts have a different
/// lifecycle than installer binaries:
///
/// - Smaller (typical KB-to-low-MB, vs MB-to-hundreds-of-MB
///   installers).
/// - Coupled to manifest versions (script lifecycle = manifest
///   lifecycle; the `script_current` / `script_status` KV gates
///   in SPEC §2.6.2 already track manifest versions, so a
///   matching dedicated bucket keeps the audit story aligned).
/// - Different access pattern (every Command execute potentially
///   fetches; vs installer fetched once per fleet deploy).
///
/// Object keys follow the same `<name>/<version>` shape as
/// `app_packages` so the SPA / operator tooling stays uniform.
/// For manifest-driven scripts `<name>` is the manifest id and
/// `<version>` is the manifest version, but the bucket itself
/// imposes no semantics on the pair — operator-uploaded
/// ad-hoc scripts can use any `<name>/<version>` they like.
pub const OBJECT_SCRIPTS: &str = "scripts";

/// Object Store holding **overflow stdout / stderr blobs** for the
/// `ExecResult` wire kind (#227). The default NATS `max_payload` is
/// 1 MB; a result whose stdout / stderr exceeds it would reject the
/// publish and pin the agent's outbox in a reconnect loop. The agent
/// uploads any stdout / stderr larger than `STDOUT_INLINE_THRESHOLD`
/// (256 KB, picked at 1/4 of the default max_payload so the rest of
/// the ExecResult fields fit alongside) into this bucket and replaces
/// the inline field with [`crate::wire::ExecResult::stdout_object`] /
/// `stderr_object` pointers. Backend's results projector derefs the
/// pointers before INSERT so downstream consumers (SQLite, SPA
/// Activity, inventory projector) see the full text the same way
/// they always have.
///
/// Object keys follow the shape `<request_id>/{stdout,stderr}` so
/// stdout + stderr for the same execution share a sibling prefix —
/// makes `kanade jetstream` listings group naturally and keeps the
/// per-key namespace tight against duplicate uploads.
///
/// Per-bucket retention (not a stream-wide TTL since async-nats
/// object_store inherits stream config): matches `STREAM_RESULTS`'s
/// 30-day retention so an operator who can still query the result
/// row in SQLite can also fetch the original blob if the inline
/// copy ever needs re-projection.
pub const OBJECT_RESULT_OUTPUT: &str = "result_output";

/// Inline threshold for `ExecResult.stdout` / `.stderr`. Larger
/// payloads overflow into [`OBJECT_RESULT_OUTPUT`]. 256 KB = 1/4 of
/// the NATS default `max_payload` (1 MB) so the rest of the
/// ExecResult JSON (request_id, exec_id, etc.) easily fits below the
/// publish-reject ceiling.
///
/// Lives next to the bucket constant rather than on the agent side
/// so the SPA / future operator tooling can quote the same threshold
/// when explaining "why this result has no inline stdout".
pub const STDOUT_INLINE_THRESHOLD: usize = 256 * 1024;

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

/// JetStream stream backing the per-PC observability event pipeline
/// (Issue #246). Distinct from [`STREAM_EVENTS`] (in-flight script
/// lifecycle) — `STREAM_OBS_EVENTS` carries the timeline data the
/// SPA's Events page consumes: sign-in/out, power on/off, sleep/
/// resume, agent milestones, diagnostic bundle pointers. The agent
/// publishes on `obs.<pc_id>` (see [`crate::subject::obs`]) and
/// this stream catches everything matching [`crate::subject::OBS_FILTER`]
/// so a backend that boots after the agent doesn't miss any
/// already-emitted events.
pub const STREAM_OBS_EVENTS: &str = "OBS_EVENTS";

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
            BUCKET_JOBS_YAML,
            BUCKET_SCHEDULES_YAML,
            OBJECT_AGENT_RELEASES,
            OBJECT_APP_PACKAGES,
            OBJECT_SCRIPTS,
            OBJECT_RESULT_OUTPUT,
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
            STREAM_OBS_EVENTS,
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
