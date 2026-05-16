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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_command() -> Command {
        Command {
            id: "echo-test".into(),
            version: "1.0.0".into(),
            request_id: "req-1".into(),
            job_id: Some("dep-1".into()),
            shell: Shell::Powershell,
            script: "echo hi".into(),
            timeout_secs: 30,
            jitter_secs: Some(5),
        }
    }

    #[test]
    fn shell_serialises_lowercase() {
        let json = serde_json::to_string(&Shell::Powershell).unwrap();
        assert_eq!(json, "\"powershell\"");
        let json = serde_json::to_string(&Shell::Cmd).unwrap();
        assert_eq!(json, "\"cmd\"");
    }

    #[test]
    fn command_round_trips_through_json() {
        let orig = sample_command();
        let json = serde_json::to_string(&orig).expect("encode");
        let decoded: Command = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.id, orig.id);
        assert_eq!(decoded.version, orig.version);
        assert_eq!(decoded.request_id, orig.request_id);
        assert_eq!(decoded.job_id, orig.job_id);
        assert_eq!(decoded.shell, orig.shell);
        assert_eq!(decoded.script, orig.script);
        assert_eq!(decoded.timeout_secs, orig.timeout_secs);
        assert_eq!(decoded.jitter_secs, orig.jitter_secs);
    }

    #[test]
    fn command_accepts_missing_optional_fields() {
        let json = r#"{
          "id": "x",
          "version": "1.0.0",
          "request_id": "r",
          "shell": "cmd",
          "script": "echo",
          "timeout_secs": 5
        }"#;
        let cmd: Command = serde_json::from_str(json).expect("decode");
        assert!(cmd.job_id.is_none());
        assert!(cmd.jitter_secs.is_none());
        assert_eq!(cmd.shell, Shell::Cmd);
    }
}
