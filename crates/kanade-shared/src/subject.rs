pub const COMMANDS_ALL: &str = "commands.all";

pub fn commands_group(name: &str) -> String {
    format!("commands.group.{name}")
}

pub fn commands_pc(pc_id: &str) -> String {
    format!("commands.pc.{pc_id}")
}

pub fn commands_deploy(job_id: &str) -> String {
    format!("commands.deploy.{job_id}")
}

pub fn results(request_id: &str) -> String {
    format!("results.{request_id}")
}

pub fn heartbeat(pc_id: &str) -> String {
    format!("heartbeat.{pc_id}")
}

pub fn kill(job_id: &str) -> String {
    format!("kill.{job_id}")
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
    fn commands_deploy_formats_job_id() {
        let job = "3c3c56b3-c83e-4c27-9fa9-4a75e1f5da6f";
        assert_eq!(
            commands_deploy(job),
            "commands.deploy.3c3c56b3-c83e-4c27-9fa9-4a75e1f5da6f"
        );
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
    fn kill_formats_job_id() {
        assert_eq!(kill("testjob1"), "kill.testjob1");
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
