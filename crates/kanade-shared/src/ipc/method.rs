//! Method-name string constants for the KLP v1 namespace (SPEC §2.12.5).
//!
//! Centralised so the agent dispatcher and client call sites refer
//! to the same string literal — a typo on either side becomes a
//! compile error instead of a silent `MethodNotFound` at runtime.
//!
//! The constants intentionally mirror the SPEC table 1:1; future
//! v2 method additions add a new entry here, never edit an existing
//! one (the protocol-version handshake in §2.12.6 is the
//! deprecation path).

// ---- system.* ----

/// Handshake — every connection's first request. SPEC §2.12.6.
pub const SYSTEM_HANDSHAKE: &str = "system.handshake";
/// Liveness check. No params, no return value beyond ack.
pub const SYSTEM_PING: &str = "system.ping";
/// Returns the agent's binary version + git rev.
pub const SYSTEM_VERSION: &str = "system.version";
/// Last N lines of `agent.log` for support handoff (SPEC §2.12.5).
pub const SYSTEM_LOG_TAIL: &str = "system.log_tail";

// ---- state.* ----

/// One-shot bundle of health + inventory + compliance checks.
pub const STATE_SNAPSHOT: &str = "state.snapshot";
/// Start streaming `state.changed` pushes for this connection.
pub const STATE_SUBSCRIBE: &str = "state.subscribe";
/// Stop streaming `state.changed` pushes (the matching unsubscribe).
pub const STATE_UNSUBSCRIBE: &str = "state.unsubscribe";
/// Push (Agent → Client) when health / vpn / version etc. flips.
pub const STATE_CHANGED: &str = "state.changed";

// ---- notifications.* ----

/// Paginated past-notifications listing (filter: unread/all).
pub const NOTIFICATIONS_LIST: &str = "notifications.list";
/// Start streaming `notifications.new` pushes for this connection.
pub const NOTIFICATIONS_SUBSCRIBE: &str = "notifications.subscribe";
/// Stop streaming `notifications.new` pushes.
pub const NOTIFICATIONS_UNSUBSCRIBE: &str = "notifications.unsubscribe";
/// Push (Agent → Client) carrying a new notification.
pub const NOTIFICATIONS_NEW: &str = "notifications.new";
/// Mark a notification read for this user — agent publishes
/// `events.notifications.acked.>` on NATS with the OS-derived SID.
pub const NOTIFICATIONS_ACK: &str = "notifications.ack";
/// Push (Agent → Client) carrying a post-send amendment to a notification
/// the client may be showing — currently a recall (delete it from screen).
pub const NOTIFICATIONS_AMENDED: &str = "notifications.amended";

// ---- jobs.* ----

/// List manifests with `user_invokable: true` (filter: category).
pub const JOBS_LIST: &str = "jobs.list";
/// Execute one of the listed jobs. Returns the synthesised `run_id`.
pub const JOBS_EXECUTE: &str = "jobs.execute";
/// Start streaming `jobs.progress` pushes for this connection.
pub const JOBS_SUBSCRIBE: &str = "jobs.subscribe";
/// Stop streaming `jobs.progress` pushes.
pub const JOBS_UNSUBSCRIBE: &str = "jobs.unsubscribe";
/// Push (Agent → Client) with stdout/stderr chunks + status changes.
pub const JOBS_PROGRESS: &str = "jobs.progress";
/// Kill a run started via `jobs.execute` on THIS connection (SPEC
/// §2.12.4 — cross-connection kill is `Unauthorized`).
pub const JOBS_KILL: &str = "jobs.kill";

// ---- support.* ----

/// Bundle recent inventory + log tail + agent state, upload to the
/// JetStream Object Store, return the resulting ticket / object id.
pub const SUPPORT_UPLOAD_DIAGNOSTICS: &str = "support.upload_diagnostics";

// ---- maintenance.* ----

/// List the next N days' scheduled jobs targeted at this PC.
pub const MAINTENANCE_LIST: &str = "maintenance.list";
/// User-initiated reboot-deferral (15m / 30m / 1h per SPEC §2.1).
pub const MAINTENANCE_DEFER: &str = "maintenance.defer";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names_are_stable_strings() {
        // Pin the wire spellings so a careless rename can't drift
        // them away from SPEC §2.12.5's table. New methods → new
        // const + new assert here.
        assert_eq!(SYSTEM_HANDSHAKE, "system.handshake");
        assert_eq!(SYSTEM_PING, "system.ping");
        assert_eq!(SYSTEM_VERSION, "system.version");
        assert_eq!(SYSTEM_LOG_TAIL, "system.log_tail");

        assert_eq!(STATE_SNAPSHOT, "state.snapshot");
        assert_eq!(STATE_SUBSCRIBE, "state.subscribe");
        assert_eq!(STATE_UNSUBSCRIBE, "state.unsubscribe");
        assert_eq!(STATE_CHANGED, "state.changed");

        assert_eq!(NOTIFICATIONS_LIST, "notifications.list");
        assert_eq!(NOTIFICATIONS_SUBSCRIBE, "notifications.subscribe");
        assert_eq!(NOTIFICATIONS_UNSUBSCRIBE, "notifications.unsubscribe");
        assert_eq!(NOTIFICATIONS_NEW, "notifications.new");
        assert_eq!(NOTIFICATIONS_ACK, "notifications.ack");
        assert_eq!(NOTIFICATIONS_AMENDED, "notifications.amended");

        assert_eq!(JOBS_LIST, "jobs.list");
        assert_eq!(JOBS_EXECUTE, "jobs.execute");
        assert_eq!(JOBS_SUBSCRIBE, "jobs.subscribe");
        assert_eq!(JOBS_UNSUBSCRIBE, "jobs.unsubscribe");
        assert_eq!(JOBS_PROGRESS, "jobs.progress");
        assert_eq!(JOBS_KILL, "jobs.kill");

        assert_eq!(SUPPORT_UPLOAD_DIAGNOSTICS, "support.upload_diagnostics");

        assert_eq!(MAINTENANCE_LIST, "maintenance.list");
        assert_eq!(MAINTENANCE_DEFER, "maintenance.defer");
    }
}
