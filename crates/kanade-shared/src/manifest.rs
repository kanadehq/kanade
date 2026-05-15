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
