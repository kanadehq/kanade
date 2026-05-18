use serde::{Deserialize, Serialize};

use crate::wire::{RunAs, Shell};

/// YAML job manifest (= registered "what to run", v0.18.0+).
///
/// Owns only script-intrinsic fields. **Who** (`target`), **how to
/// phase fanout** (`rollout`), and **when to stagger start**
/// (`jitter`) all moved to the Schedule / exec request side — same
/// script can now be fired against different targets / rollouts
/// without copying the script body.
///
/// `deny_unknown_fields` makes operators copy-pasting an older yaml
/// that still has `target:` / `rollout:` see a clear parse error at
/// `kanade job create` time instead of mysteriously losing it.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub execute: Execute,
    #[serde(default)]
    pub require_approval: bool,
    /// Opt-in marker that this job produces a JSON inventory fact
    /// payload on stdout. When present, the backend's results
    /// projector parses `ExecResult.stdout` as JSON and upserts an
    /// `inventory_facts` row keyed by `(pc_id, manifest.id)`. The
    /// `display` sub-config drives the SPA's Inventory page render.
    #[serde(default)]
    pub inventory: Option<InventoryHint>,
}

/// "Who + how + when-to-stagger" — the fanout-plan side of an exec.
/// Used both as the POST `/api/exec/{job_id}` body and as the embedded
/// `target` / `rollout` / `jitter` slot on [`Schedule`]. Centralising
/// here keeps the validation + serialisation logic in one place.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FanoutPlan {
    #[serde(default)]
    pub target: Target,
    /// Optional wave rollout — when present, the backend publishes
    /// each wave's group subject on its own delay schedule instead
    /// of fanning out the `target` block in one go. `target` then
    /// only labels the deploy for the audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<Rollout>,
    /// Optional humantime jitter; agent uses it to randomise
    /// execution start. Lives here (not on the script) so different
    /// schedules / ad-hoc fires of the same job can pick different
    /// stagger windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<String>,
}

/// Manifest sub-section: how the SPA should render the inventory
/// facts this job produces. Each field name (`field`) is a top-level
/// key in the stdout JSON, e.g. `hostname`, `ram_gb`.
///
/// Two render modes:
///   * `display` — vertical "field / value" per PC, used by the
///     `/inventory?pc=<id>` detail view. ALL columns the operator
///     wants visible on the detail page.
///   * `summary` — horizontal table across the fleet (row = PC,
///     column = field) on `/inventory`. Optional; when omitted the
///     SPA falls back to `display`, but operators usually want a
///     trimmer "hostname / OS / CPU / RAM" set for the fleet view.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InventoryHint {
    /// Detail-view columns, in order.
    pub display: Vec<DisplayField>,
    /// Optional fleet-list columns (row = PC). Defaults to `display`
    /// when omitted, but operators usually pick a 3-5 column subset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Vec<DisplayField>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DisplayField {
    /// Top-level key in the stdout JSON.
    pub field: String,
    /// Human-readable column header.
    pub label: String,
    /// Optional render hint — `"number"`, `"bytes"`, `"timestamp"`.
    /// Defaults to plain text rendering on the SPA side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
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
    /// humantime duration string (e.g. "30s", "10m"). Script-intrinsic
    /// — represents how long this script reasonably takes to run.
    pub timeout: String,
    /// Token + session combination the agent uses to launch the
    /// script (v0.21). Default = [`RunAs::System`] (Session 0,
    /// LocalSystem privileges, no GUI) — matches pre-v0.21 behavior.
    #[serde(default)]
    pub run_as: RunAs,
    /// Working directory for the spawned child (v0.21.1). When
    /// unset, the child inherits the agent's cwd — on Windows that
    /// means `%SystemRoot%\System32` for the prod service, which is
    /// almost never what operators actually want. Use an absolute
    /// path; relative paths are passed through to the OS verbatim.
    /// `%PROGRAMDATA%` works for `run_as: system`; for `run_as: user`
    /// you'd want `%USERPROFILE%` (but expansion happens in the
    /// shell, so write `$env:USERPROFILE` for PowerShell, or set
    /// it via teravars before `kanade job create`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
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
        // Matches jobs/echo-test.yaml. v0.18: no target/rollout/jitter
        // — those live on the schedule / exec request now.
        let yaml = r#"
id: echo-test
version: 0.0.1
execute:
  shell: powershell
  script: "echo 'kanade'"
  timeout: 30s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(m.id, "echo-test");
        assert_eq!(m.version, "0.0.1");
        assert!(matches!(m.execute.shell, ExecuteShell::Powershell));
        assert_eq!(m.execute.script.trim(), "echo 'kanade'");
        assert_eq!(m.execute.timeout, "30s");
        assert!(!m.require_approval);
    }

    #[test]
    fn schedule_carries_target_and_rollout() {
        let yaml = r#"
id: hourly-cleanup-canary
cron: "0 0 * * * *"
job_id: cleanup
enabled: true
target:
  groups: [canary, wave1]
jitter: 30s
rollout:
  strategy: wave
  waves:
    - { group: canary, delay: 0s }
    - { group: wave1,  delay: 5s }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.id, "hourly-cleanup-canary");
        assert_eq!(s.job_id, "cleanup");
        assert_eq!(s.plan.target.groups, vec!["canary", "wave1"]);
        assert_eq!(s.plan.jitter.as_deref(), Some("30s"));
        let rollout = s.plan.rollout.expect("rollout present");
        assert_eq!(rollout.waves.len(), 2);
        assert_eq!(rollout.waves[0].group, "canary");
        assert_eq!(rollout.waves[1].delay, "5s");
        assert_eq!(rollout.strategy, RolloutStrategy::Wave);
    }

    #[test]
    fn schedule_minimal_target_all() {
        let yaml = r#"
id: every-10s
cron: "*/10 * * * * *"
enabled: true
job_id: scheduled-echo
target: { all: true }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.id, "every-10s");
        assert_eq!(s.cron, "*/10 * * * * *");
        assert!(s.enabled);
        assert_eq!(s.job_id, "scheduled-echo");
        assert!(s.plan.target.all);
        assert!(s.plan.rollout.is_none());
        assert!(s.plan.jitter.is_none());
    }

    #[test]
    fn schedule_enabled_defaults_to_true() {
        let yaml = r#"
id: x
cron: "* * * * * *"
job_id: y
target: { all: true }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert!(s.enabled);
    }

    #[test]
    fn schedule_mode_defaults_to_every_tick() {
        let yaml = r#"
id: x
cron: "* * * * * *"
job_id: y
target: { all: true }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.mode, ExecMode::EveryTick);
        assert!(s.cooldown.is_none());
        assert!(!s.auto_disable_when_done);
    }

    #[test]
    fn schedule_mode_serialises_snake_case() {
        for (mode, expected) in [
            (ExecMode::EveryTick, "every_tick"),
            (ExecMode::OncePerPc, "once_per_pc"),
            (ExecMode::OncePerTarget, "once_per_target"),
        ] {
            let s = serde_json::to_value(mode).expect("serialise");
            assert_eq!(s, serde_json::Value::String(expected.into()));
            let back: ExecMode = serde_json::from_value(serde_json::Value::String(expected.into()))
                .expect("deserialise");
            assert_eq!(back, mode, "round-trip for {expected}");
        }
    }

    #[test]
    fn schedule_kitting_yaml_parses() {
        let yaml = r#"
id: kitting-setup
cron: "*/30 * * * * *"
job_id: install-baseline
target: { all: true }
mode: once_per_pc
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.mode, ExecMode::OncePerPc);
        assert!(s.cooldown.is_none());
        assert!(!s.auto_disable_when_done);
    }

    #[test]
    fn schedule_batch_campaign_yaml_parses() {
        let yaml = r#"
id: q3-patch-batch
cron: "*/5 * * * * *"
job_id: install-patch
target:
  pcs: [pc-001, pc-002, pc-003]
mode: once_per_pc
auto_disable_when_done: true
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.mode, ExecMode::OncePerPc);
        assert!(s.cooldown.is_none());
        assert!(s.auto_disable_when_done);
        assert_eq!(s.plan.target.pcs.len(), 3);
    }

    #[test]
    fn schedule_throttled_yaml_parses() {
        let yaml = r#"
id: daily-compliance
cron: "*/5 * * * * *"
job_id: check-av-status
target: { all: true }
mode: once_per_pc
cooldown: 1d
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.mode, ExecMode::OncePerPc);
        assert_eq!(s.cooldown.as_deref(), Some("1d"));
    }

    #[test]
    fn schedule_once_per_target_yaml_parses() {
        let yaml = r#"
id: license-checkin
cron: "*/10 * * * * *"
job_id: hit-license-server
target: { all: true }
mode: once_per_target
cooldown: 24h
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.mode, ExecMode::OncePerTarget);
        assert_eq!(s.cooldown.as_deref(), Some("24h"));
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

/// Periodic schedule (spec §2.4.3). v0.18.0 carries the fanout plan
/// (target + optional rollout + optional jitter) inline; the
/// referenced job (`job_id` → [`BUCKET_JOBS`]) supplies only the
/// script body. Two schedules of the same job can target different
/// groups on different cadences without copying the manifest.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Schedule {
    pub id: String,
    /// 6-field cron expression (`sec min hour day month day-of-week`),
    /// matching `tokio-cron-scheduler` syntax.
    pub cron: String,
    /// Key into [`crate::kv::BUCKET_JOBS`]. Must equal a registered
    /// Manifest's `id`.
    pub job_id: String,
    /// Who + how-to-phase + when-to-stagger. The Manifest doesn't
    /// carry these any more — same job + different fanout = different
    /// schedule.
    #[serde(flatten)]
    pub plan: FanoutPlan,
    /// Per-pc/per-target dedup semantics (v0.19). Default
    /// `EveryTick` keeps the historical "fire every cron tick at the
    /// whole target" behavior.
    #[serde(default)]
    pub mode: ExecMode,
    /// Humantime cooldown for `OncePerPc` / `OncePerTarget`. Once a
    /// pc/target has succeeded, the scheduler waits this long before
    /// considering it eligible again. Omit for "succeed once, then
    /// permanently skip" — i.e. cooldown = infinity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<String>,
    /// When true AND the schedule's lifecycle is permanently
    /// terminated (`cooldown = None` + dedup says nothing more to
    /// do), the scheduler flips `enabled = false` and emits an
    /// audit event. No-op when `cooldown` is set (re-arming
    /// schedules never finish).
    #[serde(default)]
    pub auto_disable_when_done: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Per-pc/per-target dedup semantics for a [`Schedule`] (v0.19).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    /// Fire on every cron tick at the whole target. Historical
    /// (pre-v0.19) behavior; no dedup.
    #[default]
    EveryTick,
    /// Fire at each pc until that pc succeeds; then skip it until
    /// the optional cooldown elapses (or forever if no cooldown).
    /// Use for kitting / first-boot / per-pc compliance checks.
    OncePerPc,
    /// Fire at the whole target until **any** pc succeeds; then
    /// skip the whole target until the optional cooldown elapses
    /// (or forever if no cooldown). Use for "one delegate is
    /// enough" tasks like license check-in.
    OncePerTarget,
}

fn default_true() -> bool {
    true
}
