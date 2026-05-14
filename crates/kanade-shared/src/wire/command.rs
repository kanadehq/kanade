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
