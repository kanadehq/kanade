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

pub const INVENTORY_HW: &str = "hw";
pub const INVENTORY_SW: &str = "sw";
pub const INVENTORY_NET: &str = "net";

/// `logs.fetch.<pc_id>` — request/reply: operator (or backend) sends
/// a `LogsRequest`; the addressed agent replies with the tail of its
/// local log file. On-demand only, no stream.
pub fn logs_fetch(pc_id: &str) -> String {
    format!("logs.fetch.{pc_id}")
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
    fn kill_formats_exec_id() {
        assert_eq!(kill("exec-uuid-1"), "kill.exec-uuid-1");
    }

    #[test]
    fn logs_fetch_formats_pc_id() {
        assert_eq!(logs_fetch("minipc"), "logs.fetch.minipc");
    }

    #[test]
    fn inventory_formats_pc_id_and_category() {
        assert_eq!(inventory("minipc", "hw"), "inventory.minipc.hw");
        assert_eq!(inventory("minipc", INVENTORY_HW), "inventory.minipc.hw");
        assert_eq!(inventory("minipc", INVENTORY_SW), "inventory.minipc.sw");
        assert_eq!(inventory("minipc", INVENTORY_NET), "inventory.minipc.net");
    }
}
