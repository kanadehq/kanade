use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Staleness;

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct Command {
    pub id: String,
    pub version: String,
    pub request_id: String,
    /// v0.29 / Issue #19: the deployment / scheduler-fire UUID this
    /// Command belongs to. Forwarded into `ExecResult.exec_id` by the
    /// agent so the projector can attribute results back to the
    /// originating `executions` row. `None` for ad-hoc `kanade run`
    /// (no deployment row exists). Pre-v0.29 wire used the field name
    /// `job_id` for this same value — `serde(alias)` keeps old
    /// publishes in STREAM_EXEC decodable across the upgrade window.
    #[serde(alias = "job_id")]
    pub exec_id: Option<String>,
    pub shell: Shell,
    pub script: String,
    pub timeout_secs: u64,
    pub jitter_secs: Option<u64>,
    /// Which (token, session) combination the agent should launch the
    /// child process under (v0.21). Defaults to [`RunAs::System`] for
    /// back-compat with pre-v0.21 backends that don't send this field.
    #[serde(default)]
    pub run_as: RunAs,
    /// Working directory for the spawned child (v0.21.1). `None` ⇒
    /// inherit the agent's cwd. Pre-v0.21.1 wire payloads omit this
    /// field and parse fine via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Absolute time after which the agent should refuse to run
    /// this Command (v0.22). Set by the scheduler from
    /// `Schedule.starting_deadline` (humantime) measured against
    /// the cron tick time. `None` ⇒ no deadline, run whenever
    /// received (default for ad-hoc `kanade exec` + back-compat
    /// for pre-v0.22 wire). The agent stamps a synthetic
    /// `ExecResult { exit_code: 125, stderr: "skipped: deadline
    /// expired ..." }` when it skips, so the operator sees the
    /// outcome on the Results / Dashboard pages instead of silence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<DateTime<Utc>>,
    /// v0.26: Manifest-declared Layer 2 staleness policy
    /// (see SPEC.md §2.6.2). Forwarded from `Manifest.staleness` so
    /// the agent can evaluate it at fire time without re-fetching the
    /// Manifest from `BUCKET_JOBS`. Pre-v0.26 wire omits this and
    /// `#[serde(default)]` falls back to `Staleness::Cached`, matching
    /// pre-v0.26 behaviour (silently use cached KV values).
    #[serde(default)]
    pub staleness: Staleness,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Powershell,
    Cmd,
}

/// **Token + session combination** the agent uses to spawn a job's
/// child process. Two orthogonal axes — *whose privileges* and *which
/// session* — collapse into three meaningful combinations:
///
/// | variant            | session                | privileges  | GUI |
/// |--------------------|------------------------|-------------|-----|
/// | `System` (default) | Session 0 (services)   | LocalSystem | ❌  |
/// | `User`             | active console session | logged-in user (UAC-filtered when admin) | ✅ |
/// | `SystemGui`        | active console session | LocalSystem | ✅  |
///
/// `SystemGui` is the "PsExec `-i -s`" pattern: the agent duplicates
/// its own SYSTEM token and rewrites `TokenSessionId` to the user's
/// console session, then launches with that hybrid token — useful
/// when an installer needs admin power *and* needs the user to see
/// its UI.
#[derive(
    Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum RunAs {
    /// LocalSystem privileges in Session 0. No GUI. Historical
    /// default — every pre-v0.21 job ran this way.
    #[default]
    System,
    /// The currently-logged-in console user's identity, in their
    /// session. Can write HKCU / %APPDATA% / show GUI to the user.
    /// Privileges are whatever the user has (admin users get the
    /// UAC-filtered limited token, not the elevated one).
    User,
    /// LocalSystem privileges in the user's session — admin power
    /// with GUI visibility. Niche but real (force-restart dialogs,
    /// admin installers with progress UI).
    SystemGui,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_command() -> Command {
        Command {
            id: "echo-test".into(),
            version: "1.0.0".into(),
            request_id: "req-1".into(),
            exec_id: Some("dep-1".into()),
            shell: Shell::Powershell,
            script: "echo hi".into(),
            timeout_secs: 30,
            jitter_secs: Some(5),
            run_as: RunAs::System,
            cwd: None,
            deadline_at: None,
            staleness: Staleness::Cached,
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
    fn run_as_serialises_snake_case() {
        for (mode, expected) in [
            (RunAs::System, "\"system\""),
            (RunAs::User, "\"user\""),
            (RunAs::SystemGui, "\"system_gui\""),
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, expected, "serialise {mode:?}");
            let back: RunAs = serde_json::from_str(expected).unwrap();
            assert_eq!(back, mode, "round-trip {expected}");
        }
    }

    #[test]
    fn run_as_defaults_to_system() {
        assert_eq!(RunAs::default(), RunAs::System);
    }

    #[test]
    fn command_round_trips_through_json() {
        let orig = sample_command();
        let json = serde_json::to_string(&orig).expect("encode");
        let decoded: Command = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.id, orig.id);
        assert_eq!(decoded.version, orig.version);
        assert_eq!(decoded.request_id, orig.request_id);
        assert_eq!(decoded.exec_id, orig.exec_id);
        assert_eq!(decoded.shell, orig.shell);
        assert_eq!(decoded.script, orig.script);
        assert_eq!(decoded.timeout_secs, orig.timeout_secs);
        assert_eq!(decoded.jitter_secs, orig.jitter_secs);
        assert_eq!(decoded.run_as, orig.run_as);
    }

    #[test]
    fn command_round_trips_each_run_as_variant() {
        for mode in [RunAs::System, RunAs::User, RunAs::SystemGui] {
            let cmd = Command {
                run_as: mode,
                ..sample_command()
            };
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(back.run_as, mode);
        }
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
        assert!(cmd.exec_id.is_none());
        assert!(cmd.jitter_secs.is_none());
        assert_eq!(cmd.shell, Shell::Cmd);
        // Pre-v0.21 wire payloads omit run_as → falls back to System.
        assert_eq!(cmd.run_as, RunAs::System);
        // Pre-v0.21.1 omit cwd → None (= inherit agent cwd).
        assert!(cmd.cwd.is_none());
        // Pre-v0.22 omit deadline_at → None (= no deadline).
        assert!(cmd.deadline_at.is_none());
    }

    #[test]
    fn command_decodes_legacy_job_id_field_as_exec_id() {
        // v0.29 / Issue #19: Commands sitting in STREAM_EXEC published
        // by a pre-v0.29 backend still carry the field named `job_id`.
        // The `#[serde(alias = "job_id")]` on `exec_id` keeps them
        // decodable through the upgrade window so the agent doesn't
        // start dropping replays on first boot of a new binary.
        let json = r#"{
          "id": "x",
          "version": "1.0.0",
          "request_id": "r",
          "job_id": "legacy-exec-uuid",
          "shell": "powershell",
          "script": "echo",
          "timeout_secs": 5
        }"#;
        let cmd: Command = serde_json::from_str(json).expect("decode legacy");
        assert_eq!(cmd.exec_id.as_deref(), Some("legacy-exec-uuid"));
    }

    #[test]
    fn command_deadline_at_round_trips() {
        use chrono::TimeZone;
        let deadline = Utc.with_ymd_and_hms(2026, 5, 18, 9, 30, 0).unwrap();
        let cmd = Command {
            deadline_at: Some(deadline),
            ..sample_command()
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(back.deadline_at, Some(deadline));
    }
}
