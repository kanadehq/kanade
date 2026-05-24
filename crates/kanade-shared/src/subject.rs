pub const COMMANDS_ALL: &str = "commands.all";

pub fn commands_group(name: &str) -> String {
    format!("commands.group.{name}")
}

pub fn commands_pc(pc_id: &str) -> String {
    format!("commands.pc.{pc_id}")
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

pub const INVENTORY_HW: &str = "hw";
pub const INVENTORY_SW: &str = "sw";
pub const INVENTORY_NET: &str = "net";

/// `logs.fetch.<pc_id>` — request/reply: operator (or backend) sends
/// a `LogsRequest`; the addressed agent replies with the tail of its
/// local log file. On-demand only, no stream.
pub fn logs_fetch(pc_id: &str) -> String {
    format!("logs.fetch.{pc_id}")
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
        assert_eq!(commands_pc("minipc"), "commands.pc.minipc");
        assert_eq!(commands_pc("PC1234"), "commands.pc.PC1234");
    }

    #[test]
    fn results_formats_request_id() {
        assert_eq!(results("req-1"), "results.req-1");
    }

    #[test]
    fn heartbeat_formats_pc_id() {
        assert_eq!(heartbeat("minipc"), "heartbeat.minipc");
    }

    #[test]
    fn host_perf_formats_pc_id() {
        assert_eq!(host_perf("minipc"), "host_perf.minipc");
        assert_eq!(host_perf("PC1234"), "host_perf.PC1234");
    }

    #[test]
    fn process_perf_formats_pc_id() {
        assert_eq!(process_perf("minipc"), "process_perf.minipc");
        assert_eq!(process_perf("PC1234"), "process_perf.PC1234");
    }

    #[test]
    fn kill_formats_exec_id() {
        assert_eq!(kill("exec-uuid-1"), "kill.exec-uuid-1");
    }

    #[test]
    fn logs_fetch_formats_pc_id() {
        assert_eq!(logs_fetch("minipc"), "logs.fetch.minipc");
    }

    #[test]
    fn ping_formats_pc_id() {
        assert_eq!(ping("minipc"), "agents.minipc.ping");
    }

    #[test]
    fn events_started_formats_exec_id_and_pc_id() {
        assert_eq!(
            events_started("exec-uuid-1", "minipc"),
            "events.started.exec-uuid-1.minipc",
        );
    }

    #[test]
    fn events_started_filter_is_narrow_wildcard() {
        assert_eq!(EVENTS_STARTED_FILTER, "events.started.>");
    }

    #[test]
    fn inventory_formats_pc_id_and_category() {
        assert_eq!(inventory("minipc", "hw"), "inventory.minipc.hw");
        assert_eq!(inventory("minipc", INVENTORY_HW), "inventory.minipc.hw");
        assert_eq!(inventory("minipc", INVENTORY_SW), "inventory.minipc.sw");
        assert_eq!(inventory("minipc", INVENTORY_NET), "inventory.minipc.net");
    }
}
