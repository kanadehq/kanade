use serde::{Deserialize, Serialize};

use crate::wire::Shell;

/// YAML job manifest (spec §2.4.1, Sprint 4a covers everything except
/// `execute.script_file` / `execute.script_object` / `on_failure`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub target: Target,
    pub execute: Execute,
    /// Optional wave rollout — when present, the backend publishes each
    /// wave's group subject on its own delay schedule instead of fanning
    /// out the `target` block at deploy time. `target` is then only used
    /// as a fallback (e.g. `target.all: true` to mark the manifest as
    /// fleet-wide for the audit log).
    #[serde(default)]
    pub rollout: Option<Rollout>,
    #[serde(default)]
    pub require_approval: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Rollout {
    #[serde(default)]
    pub strategy: RolloutStrategy,
    pub waves: Vec<Wave>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RolloutStrategy {
    #[default]
    Wave,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Wave {
    pub group: String,
    /// humantime delay measured from the deploy's publish time. wave[0]
    /// typically has "0s"; subsequent waves use minutes / hours.
    pub delay: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Target {
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub pcs: Vec<String>,
    #[serde(default)]
    pub all: bool,
}

impl Target {
    /// At least one of all / groups / pcs is set.
    pub fn is_specified(&self) -> bool {
        self.all || !self.groups.is_empty() || !self.pcs.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Execute {
    pub shell: ExecuteShell,
    pub script: String,
    /// humantime duration string (e.g. "30s", "10m").
    pub timeout: String,
    /// Optional humantime jitter; agent uses it to randomise execution start.
    #[serde(default)]
    pub jitter: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecuteShell {
    Powershell,
    Cmd,
}

impl From<ExecuteShell> for Shell {
    fn from(s: ExecuteShell) -> Self {
        match s {
            ExecuteShell::Powershell => Shell::Powershell,
            ExecuteShell::Cmd => Shell::Cmd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_is_specified_requires_at_least_one_field() {
        let empty = Target::default();
        assert!(!empty.is_specified());

        let with_all = Target {
            all: true,
            ..Target::default()
        };
        assert!(with_all.is_specified());

        let with_groups = Target {
            groups: vec!["canary".into()],
            ..Target::default()
        };
        assert!(with_groups.is_specified());

        let with_pcs = Target {
            pcs: vec!["minipc".into()],
            ..Target::default()
        };
        assert!(with_pcs.is_specified());
    }

    #[test]
    fn manifest_deserialises_minimal_yaml() {
        // Matches jobs/echo-test.yaml.
        let yaml = r#"
id: echo-test
version: 0.0.1
target:
  pcs: [minipc]
execute:
  shell: powershell
  script: "echo 'kanade'"
  timeout: 30s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(m.id, "echo-test");
        assert_eq!(m.version, "0.0.1");
        assert!(m.target.is_specified());
        assert_eq!(m.target.pcs, vec!["minipc"]);
        assert!(matches!(m.execute.shell, ExecuteShell::Powershell));
        assert_eq!(m.execute.script.trim(), "echo 'kanade'");
        assert_eq!(m.execute.timeout, "30s");
        assert!(m.execute.jitter.is_none());
        assert!(m.rollout.is_none());
        assert!(!m.require_approval);
    }

    #[test]
    fn manifest_deserialises_wave_rollout() {
        let yaml = r#"
id: cleanup
version: 1.0.0
target:
  groups: [canary, wave1]
execute:
  shell: cmd
  script: "rmdir /S /Q C:\\temp"
  timeout: 5m
  jitter: 30s
rollout:
  strategy: wave
  waves:
    - { group: canary, delay: 0s }
    - { group: wave1,  delay: 5s }
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert!(matches!(m.execute.shell, ExecuteShell::Cmd));
        assert_eq!(m.execute.jitter.as_deref(), Some("30s"));
        let rollout = m.rollout.expect("rollout present");
        assert_eq!(rollout.waves.len(), 2);
        assert_eq!(rollout.waves[0].group, "canary");
        assert_eq!(rollout.waves[0].delay, "0s");
        assert_eq!(rollout.waves[1].delay, "5s");
        assert_eq!(rollout.strategy, RolloutStrategy::Wave);
    }

    #[test]
    fn schedule_embeds_full_manifest() {
        let yaml = r#"
id: every-10s
cron: "*/10 * * * * *"
enabled: true
manifest:
  id: scheduled-echo
  version: 1.0.0
  target:
    pcs: [minipc]
  execute:
    shell: powershell
    script: "echo hi"
    timeout: 30s
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.id, "every-10s");
        assert_eq!(s.cron, "*/10 * * * * *");
        assert!(s.enabled);
        assert_eq!(s.manifest.id, "scheduled-echo");
    }

    #[test]
    fn schedule_enabled_defaults_to_true() {
        let yaml = r#"
id: x
cron: "* * * * * *"
manifest:
  id: y
  version: 1.0.0
  target:
    all: true
  execute:
    shell: powershell
    script: "echo"
    timeout: 1s
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert!(s.enabled);
    }

    #[test]
    fn execute_shell_into_wire_shell() {
        assert_eq!(Shell::from(ExecuteShell::Powershell), Shell::Powershell);
        assert_eq!(Shell::from(ExecuteShell::Cmd), Shell::Cmd);
    }

    #[test]
    fn missing_required_field_errors() {
        // `id` missing.
        let yaml = r#"
version: 1.0.0
target: { all: true }
execute:
  shell: powershell
  script: "echo"
  timeout: 1s
"#;
        let r: Result<Manifest, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err(), "expected error, got {:?}", r);
    }
}

/// Periodic schedule (spec §2.4.3). The full job [`Manifest`] is embedded
/// so the scheduler can deploy it without a separate Git lookup; once a
/// dedicated job-catalog API lands, `manifest` can become a `job_id`
/// reference instead.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Schedule {
    pub id: String,
    /// 6-field cron expression (`sec min hour day month day-of-week`),
    /// matching `tokio-cron-scheduler` syntax.
    pub cron: String,
    pub manifest: Manifest,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}
