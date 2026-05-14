pub const COMMANDS_ALL: &str = "commands.all";

pub fn commands_group(name: &str) -> String {
    format!("commands.group.{name}")
}

pub fn commands_pc(pc_id: &str) -> String {
    format!("commands.pc.{pc_id}")
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
