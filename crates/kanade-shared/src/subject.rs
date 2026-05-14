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
