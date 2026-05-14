pub mod config;
pub mod subject;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Command {
    pub id: String,
    pub version: String,
    pub request_id: String,
    pub job_id: Option<String>,
    pub shell: Shell,
    pub script: String,
    pub timeout_secs: u64,
    pub jitter_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Powershell,
    Cmd,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecResult {
    pub request_id: String,
    pub pc_id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Heartbeat {
    pub pc_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub agent_version: String,
}
