pub const COMMANDS_ALL: &str = "commands.all";

pub fn commands_group(name: &str) -> String {
    format!("commands.group.{name}")
}

pub fn commands_pc(pc_id: &str) -> String {
    format!("commands.pc.{pc_id}")
}

/// `notifications.all` — broadcast end-user notification (SPEC
/// §2.2.1 / Phase E). Mirrors [`COMMANDS_ALL`] but on the
/// notification fan-out plane: the backend publishes here, the
/// `NOTIFICATIONS` stream retains it, and every agent forwards it to
/// the Client Apps in matching user sessions.
pub const NOTIFICATIONS_ALL: &str = "notifications.all";

/// Subject prefix for [`notifications_group`]. Exposed so callers that
/// parse a subject back into its group name (the backend's notification
/// audience resolver) strip against the same string the builder emits —
/// if the format ever changes, both move together instead of the parser
/// silently failing to match.
pub const NOTIFICATIONS_GROUP_PREFIX: &str = "notifications.group.";

/// Subject prefix for [`notifications_pc`]. See
/// [`NOTIFICATIONS_GROUP_PREFIX`].
pub const NOTIFICATIONS_PC_PREFIX: &str = "notifications.pc.";

/// `notifications.group.{group_name}` — group-scoped end-user
/// notification. Sibling of [`commands_group`] on the notification
/// plane.
pub fn notifications_group(name: &str) -> String {
    format!("{NOTIFICATIONS_GROUP_PREFIX}{name}")
}

/// `notifications.pc.{pc_id}` — single-PC end-user notification.
/// Sibling of [`commands_pc`] on the notification plane.
pub fn notifications_pc(pc_id: &str) -> String {
    format!("{NOTIFICATIONS_PC_PREFIX}{pc_id}")
}

/// `events.notifications.acked.{pc_id}.{user_sid}.{notif_id}` — the
/// agent publishes this when a user clicks "確認" on a notification
/// (SPEC §2.2.2 / Phase E). The `{user_sid}` segment distinguishes
/// concurrent users on a shared PC (Fast User Switching / RDP). Lives
/// under `events.>` so the existing `EVENTS` stream retains it; the
/// backend's notification-acks projector consumes the narrowed
/// [`EVENTS_NOTIFICATIONS_ACKED_FILTER`] to build the SPA's
/// per-recipient confirmation view.
///
/// Subject spelling is fixed by SPEC §2.2.2 / §2.12.8 (`events.>`), so
/// acks ride the `EVENTS` stream's retention (shorter than the 90-day
/// `NOTIFICATIONS` history), not the notification stream's. That only
/// bounds **re-projection** after a `-WipeDb`: the durable source of
/// truth for ack_status is the `notification_acks` SQLite table, which
/// persists independently — so a live fleet keeps full ack history;
/// only a DB wipe truncates re-derivable acks to the EVENTS window,
/// the same limitation every `events.*`-projected table already has.
pub fn events_notifications_acked(pc_id: &str, user_sid: &str, notif_id: &str) -> String {
    format!("events.notifications.acked.{pc_id}.{user_sid}.{notif_id}")
}

// `commands_exec` (subject `commands.exec.<job_id>`) was removed in
// v0.22.1. The STREAM_EXEC stream now catches the existing
// `commands.{all,group.X,pc.Y}` subjects directly, so the dedicated
// per-exec subject isn't needed any more. See
// `kanade-agent::command_replay` for how reconnecting agents catch
// up on missed messages.

pub fn results(request_id: &str) -> String {
    format!("results.{request_id}")
}

pub fn heartbeat(pc_id: &str) -> String {
    format!("heartbeat.{pc_id}")
}

/// `host_perf.<pc_id>` — Phase 1 of the perf telemetry pipeline. The
/// agent publishes a whole-host CPU / Memory / Disk I/O / Network
/// snapshot here on the cadence set by `host_perf_interval` in
/// agent_config (default 60 s). Distinct subject from `heartbeat.<pc_id>`
/// so the periodic heartbeat publisher stays untouched and pre-host_perf
/// backends that don't subscribe simply ignore the new traffic.
pub fn host_perf(pc_id: &str) -> String {
    format!("host_perf.{pc_id}")
}

/// `process_perf.<pc_id>` — Phase 2: top-N per-process snapshot
/// published only while `process_perf_enabled` is `true` AND the
/// `process_perf_expires_at` deadline is in the future. Separate
/// subject from `host_perf.<pc_id>` because process-perf is an
/// opt-in investigation mode — having its own subject lets the
/// projector skip the heavy table entirely for hosts that never
/// turn it on.
pub fn process_perf(pc_id: &str) -> String {
    format!("process_perf.{pc_id}")
}

/// `obs.<pc_id>` — per-PC observability event stream (Issue #246).
/// The agent publishes one [`crate::wire::ObsEvent`] per timeline
/// event (sign-in / out, power on / off, sleep / resume, agent
/// milestones, diagnostic bundle pointers). Distinct from
/// `events.started.*` (in-flight script lifecycle) and
/// `host_perf.<pc_id>` (numeric telemetry) — `obs.*` is the
/// semantic-event stream the SPA Timeline page consumes.
pub fn obs(pc_id: &str) -> String {
    format!("obs.{pc_id}")
}

/// `obs.>` — filter the backend projector subscribes to so a new
/// PC starts flowing into the timeline without any per-PC SUB
/// registration. Pairs with [`obs`] for publish.
pub const OBS_FILTER: &str = "obs.>";

/// `kill.<exec_id>` — Spec §2.6 Layer 3 abort signal. The exec_id is
/// the deployment / scheduler-fire UUID (formerly named `job_id`
/// pre-v0.29; renamed for accuracy — every `Command.exec_id` is a
/// per-deploy UUID, not a job-catalog id).
pub fn kill(exec_id: &str) -> String {
    format!("kill.{exec_id}")
}

pub fn inventory(pc_id: &str, category: &str) -> String {
    format!("inventory.{pc_id}.{category}")
}

/// `events.started.<exec_id>.<pc_id>` — v0.30 / PR α' lifecycle
/// event published by the agent just before spawning a script's
/// child process. Lets the backend project an in-flight row into
/// `execution_results` (with `finished_at = NULL`) so the SPA
/// Activity table can show running rows alongside finished ones.
/// Backend subscribes via [`EVENTS_STARTED_FILTER`].
pub fn events_started(exec_id: &str, pc_id: &str) -> String {
    format!("events.started.{exec_id}.{pc_id}")
}

/// Wildcard the backend events projector consumes on STREAM_EVENTS.
/// Narrow (`events.started.>`) rather than the whole `events.>` so
/// future event types can carry their own filters without rerouting
/// the started subset.
pub const EVENTS_STARTED_FILTER: &str = "events.started.>";

/// Wildcard the backend notification-acks projector consumes on
/// `STREAM_EVENTS`. Narrow (`events.notifications.acked.>`) rather
/// than the whole `events.>` so the projector only wakes for ack
/// events and not the high-volume `events.started.*` lifecycle
/// traffic (which the events projector handles separately).
pub const EVENTS_NOTIFICATIONS_ACKED_FILTER: &str = "events.notifications.acked.>";

pub const INVENTORY_HW: &str = "hw";
pub const INVENTORY_SW: &str = "sw";
pub const INVENTORY_NET: &str = "net";

/// `logs.fetch.<pc_id>` — request/reply: operator (or backend) sends
/// a `LogsRequest`; the addressed agent replies with the tail of its
/// local log file. On-demand only, no stream.
pub fn logs_fetch(pc_id: &str) -> String {
    format!("logs.fetch.{pc_id}")
}

/// `job.tail.<pc_id>` — request/reply for the live tail of a
/// still-running job's stdout/stderr. The operator (or backend, on
/// the SPA's behalf) sends a [`crate::wire::JobTailRequest`] carrying
/// the `result_id`; the addressed agent replies with the current
/// ring-buffer tail from its in-memory live registry. On-demand only,
/// no stream — the SPA polls this every few seconds (same shape as
/// `logs.fetch.<pc_id>`) while a job is in flight. Distinct subject
/// from `logs.fetch.<pc_id>` (whole-agent log file) because this is
/// scoped to a single job's captured output, not the agent's log.
pub fn job_tail(pc_id: &str) -> String {
    format!("job.tail.{pc_id}")
}

/// `agents.<pc_id>.ping` — v0.38 / #133 request/reply for the
/// active "ping" round-trip. The agent answers with a fresh
/// `Heartbeat` on demand instead of the backend waiting up to ~30 s
/// for the next periodic heartbeat tick to land. Distinct subject
/// from `heartbeat.<pc_id>` so the periodic publisher is unaffected
/// and old agents that don't subscribe simply time the request out.
pub fn ping(pc_id: &str) -> String {
    format!("agents.{pc_id}.ping")
}

// v0.14: subject::inventory_request was retired alongside the
// hardcoded inventory loop. On-demand collection now goes through
// the normal exec path (`kanade exec configs/jobs/inventory-
// hw.yaml`) — Command + ExecResult + the inventory-fact projector
// give operators the same effect with no extra subject.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_all_constant() {
        assert_eq!(COMMANDS_ALL, "commands.all");
    }

    #[test]
    fn commands_group_formats_name() {
        assert_eq!(commands_group("canary"), "commands.group.canary");
        assert_eq!(commands_group("wave1"), "commands.group.wave1");
    }

    #[test]
    fn commands_pc_formats_id() {
        assert_eq!(commands_pc("pc-01"), "commands.pc.pc-01");
        assert_eq!(commands_pc("PC1234"), "commands.pc.PC1234");
    }

    #[test]
    fn notifications_all_constant() {
        assert_eq!(NOTIFICATIONS_ALL, "notifications.all");
    }

    #[test]
    fn notifications_group_formats_name() {
        assert_eq!(
            notifications_group("tokyo-office"),
            "notifications.group.tokyo-office"
        );
    }

    #[test]
    fn notifications_pc_formats_id() {
        assert_eq!(notifications_pc("PC1234"), "notifications.pc.PC1234");
    }

    #[test]
    fn events_notifications_acked_formats_all_segments() {
        assert_eq!(
            events_notifications_acked("PC1234", "S-1-5-21-1001", "notif-9f3a"),
            "events.notifications.acked.PC1234.S-1-5-21-1001.notif-9f3a"
        );
    }

    #[test]
    fn events_notifications_acked_filter_is_narrow_wildcard() {
        assert_eq!(
            EVENTS_NOTIFICATIONS_ACKED_FILTER,
            "events.notifications.acked.>"
        );
        // Must stay a strict subset of the EVENTS stream's `events.>`
        // subjects so STREAM_EVENTS retains it without a config change.
        assert!(EVENTS_NOTIFICATIONS_ACKED_FILTER.starts_with("events."));
    }

    #[test]
    fn results_formats_request_id() {
        assert_eq!(results("req-1"), "results.req-1");
    }

    #[test]
    fn heartbeat_formats_pc_id() {
        assert_eq!(heartbeat("pc-01"), "heartbeat.pc-01");
    }

    #[test]
    fn host_perf_formats_pc_id() {
        assert_eq!(host_perf("pc-01"), "host_perf.pc-01");
        assert_eq!(host_perf("PC1234"), "host_perf.PC1234");
    }

    #[test]
    fn process_perf_formats_pc_id() {
        assert_eq!(process_perf("pc-01"), "process_perf.pc-01");
        assert_eq!(process_perf("PC1234"), "process_perf.PC1234");
    }

    #[test]
    fn obs_formats_pc_id() {
        assert_eq!(obs("pc-01"), "obs.pc-01");
        assert_eq!(obs("PC1234"), "obs.PC1234");
    }

    #[test]
    fn obs_filter_constant() {
        assert_eq!(OBS_FILTER, "obs.>");
    }

    #[test]
    fn kill_formats_exec_id() {
        assert_eq!(kill("exec-uuid-1"), "kill.exec-uuid-1");
    }

    #[test]
    fn logs_fetch_formats_pc_id() {
        assert_eq!(logs_fetch("pc-01"), "logs.fetch.pc-01");
    }

    #[test]
    fn ping_formats_pc_id() {
        assert_eq!(ping("pc-01"), "agents.pc-01.ping");
    }

    #[test]
    fn job_tail_formats_pc_id() {
        assert_eq!(job_tail("pc-01"), "job.tail.pc-01");
        assert_eq!(job_tail("PC1234"), "job.tail.PC1234");
    }

    #[test]
    fn events_started_formats_exec_id_and_pc_id() {
        assert_eq!(
            events_started("exec-uuid-1", "pc-01"),
            "events.started.exec-uuid-1.pc-01",
        );
    }

    #[test]
    fn events_started_filter_is_narrow_wildcard() {
        assert_eq!(EVENTS_STARTED_FILTER, "events.started.>");
    }

    #[test]
    fn inventory_formats_pc_id_and_category() {
        assert_eq!(inventory("pc-01", "hw"), "inventory.pc-01.hw");
        assert_eq!(inventory("pc-01", INVENTORY_HW), "inventory.pc-01.hw");
        assert_eq!(inventory("pc-01", INVENTORY_SW), "inventory.pc-01.sw");
        assert_eq!(inventory("pc-01", INVENTORY_NET), "inventory.pc-01.net");
    }
}
