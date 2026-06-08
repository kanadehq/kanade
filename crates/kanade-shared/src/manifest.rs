use serde::{Deserialize, Serialize};

use crate::wire::{RunAs, Shell, Staleness};

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
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
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
    /// Issue #246: opt-in marker that this job emits per-line
    /// observability events on stdout (one JSON `ObsEvent` per
    /// newline). When present, the agent — after the script exits
    /// successfully — parses each non-empty stdout line as an
    /// `ObsEvent`, publishes it on `obs.<pc_id>` via the
    /// `obs_outbox`, and (intentionally) **omits the stdout from
    /// the `ExecResult`** so the timeline data doesn't double up
    /// in `execution_results.stdout` (which would multiply rows
    /// by ~50/day/PC of noise).
    ///
    /// Distinct from `inventory:` (single JSON object → projector
    /// upsert) — events are append-only timeline points consumed
    /// by the dedicated `obs_events` table.
    #[serde(default)]
    pub emit: Option<EmitConfig>,
    /// v0.26: Layer 2 staleness policy (SPEC.md §2.6.2). Controls
    /// what the agent does at fire time when it can't verify the
    /// `script_current` / `script_status` KV values are fresh —
    /// especially relevant for `runs_on: agent` schedules where
    /// the agent may fire from cache while offline. Defaults to
    /// `Staleness::Cached` (silently use cached values), which
    /// matches every pre-v0.26 Manifest.
    #[serde(default)]
    pub staleness: Staleness,
}

/// "Who + how + when-to-stagger" — the fanout-plan side of an exec.
/// Used both as the POST `/api/exec/{job_id}` body and as the embedded
/// `target` / `rollout` / `jitter` slot on [`Schedule`]. Centralising
/// here keeps the validation + serialisation logic in one place.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
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
    /// Absolute time the scheduler stamps on each emitted Command
    /// when this exec was driven by a [`Schedule`] with
    /// `starting_deadline`. Agents receiving a Command after this
    /// instant publish a synthetic skipped-result instead of
    /// running the script. `None` (default) = no deadline / catch
    /// up whenever delivered. Operators don't usually set this
    /// directly — the scheduler computes it from `tick_at +
    /// starting_deadline`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<chrono::DateTime<chrono::Utc>>,
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
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct InventoryHint {
    /// Detail-view columns, in order.
    pub display: Vec<DisplayField>,
    /// Optional fleet-list columns (row = PC). Defaults to `display`
    /// when omitted, but operators usually pick a 3-5 column subset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Vec<DisplayField>>,
    /// v0.31 / #40: payload arrays that should be exploded into
    /// per-element rows of a derived SQLite table. Lets operators
    /// answer cross-PC questions ("which PCs still have Chrome <
    /// 120?", "C: >90% full") with normal SQL filters + indexes
    /// instead of grepping JSON. The projector creates the derived
    /// table on register and replaces this PC's rows on each result
    /// (DELETE WHERE pc_id=? AND job_id=? + bulk INSERT). See
    /// [`ExplodeSpec`] for the per-spec schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explode: Option<Vec<ExplodeSpec>>,
    /// v0.35 / #93: top-level scalar fields whose changes the
    /// projector logs to `inventory_history` (one event per
    /// changed field per scan). Pairs with `explode[].track_history`
    /// — that covers array elements; this covers single-valued
    /// fields like `ram_bytes` / `os_version` / `cpu_model` /
    /// `os_build` that operators want to track for "did the RAM
    /// get upgraded?" / "when did Win 11 land on this PC?" /
    /// "BIOS / firmware bumped?" questions. Field name = `field_path`
    /// in the history row, `identity_json` is NULL, `before_json`
    /// / `after_json` each carry `{"value": <prior or new value>}`.
    /// First-ever observation of a scalar (no prior facts row)
    /// emits `added`; subsequent value changes emit `changed`. No
    /// `removed` events — a scalar disappearing from the payload
    /// is rare and the operator can still see the last value via
    /// the `before_json` of the most recent change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_scalars: Option<Vec<String>>,
}

/// Issue #246 — `emit:` manifest block for jobs whose stdout is
/// NDJSON observability events (one `ObsEvent` per line). Parallel
/// to `inventory:` but for the append-only timeline pipeline; see
/// `Manifest::emit` for the full contract.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct EmitConfig {
    /// What kind of payload the agent should expect on stdout. Only
    /// `events` is defined today (parses each non-empty line as
    /// `ObsEvent` and publishes on `obs.<pc_id>`); future variants
    /// (e.g. metrics streams, structured trace events) plug in here.
    #[serde(rename = "type")]
    pub kind: EmitKind,
    /// Operator hint for where the script keeps its own state — the
    /// watermark file the PowerShell / sh body reads + writes
    /// between runs so it only emits NEW events since the last
    /// poll. The agent doesn't read this; it's documentation that
    /// the SPA (and `kanade job edit`) can surface to operators
    /// reviewing the manifest. Optional; the script is allowed to
    /// keep state anywhere (registry, env, etc.) — the field's
    /// presence makes the convention discoverable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark_path: Option<String>,
}

/// `emit.type` enum. Lowercase serde so manifests read
/// `type: events` rather than `Events`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmitKind {
    /// Per-line `ObsEvent` JSON. Agent parses + publishes on
    /// `obs.<pc_id>`, drops the stdout from the resulting
    /// `ExecResult`.
    Events,
}

/// v0.31 / #40: declarative "flatten this JSON array into a real
/// SQLite table" spec on an inventory manifest. The projector
/// creates the table on first registration (CREATE TABLE IF NOT
/// EXISTS + indexes) and writes a row per element of
/// `payload[field]` on every result, scoped by (pc_id, job_id) so
/// each PC's rows replace cleanly without a per-PC schema.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct ExplodeSpec {
    /// JSON array key under the payload to explode. E.g. `"apps"`
    /// for `payload: { apps: [{...}, {...}] }`.
    pub field: String,
    /// Derived SQLite table name. Operators choose this — pick
    /// something namespaced + stable (`inventory_sw_apps`, not
    /// `apps`) so multiple inventory manifests don't collide on a
    /// generic name.
    pub table: String,
    /// Element-level fields that uniquely identify a row inside one
    /// PC's payload. The full PK is `(pc_id, job_id) + these
    /// columns`. Required — operators must think about uniqueness
    /// (e.g. `["name", "source"]` for installed apps because the
    /// same name appears in multiple uninstall hives).
    ///
    /// v0.31 / #41: same tuple drives history identity. When
    /// `track_history` is on, the projector serialises these
    /// fields' values into `inventory_history.identity_json` for
    /// every change event, so queries like "every PC that ever
    /// installed Chrome (any source)" filter on identity_json
    /// content without a per-manifest schema.
    pub primary_key: Vec<String>,
    /// Per-element fields that become columns in the derived table.
    pub columns: Vec<ExplodeColumn>,
    /// v0.31 / #41: when true (default false), the projector
    /// diffs each PC's incoming payload against the prior rows
    /// for the same (pc_id, job_id) BEFORE the DELETE-then-INSERT
    /// replace, and writes added / removed / changed events into
    /// `inventory_history`. Lets operators answer time-dimension
    /// questions ("when did Chrome 120 first appear on PC X?",
    /// "what's the Win 11 23H2 rollout curve") without storing
    /// per-scan snapshots. Off by default so operators opt in
    /// per-spec — history has a real storage cost on long-lived
    /// deployments (mitigated by the 90-day default retention
    /// sweeper, see `cleanup` module).
    #[serde(default)]
    pub track_history: bool,
}

/// One column in an [`ExplodeSpec`]'s derived table.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct ExplodeColumn {
    /// JSON key under each array element. Becomes the column name
    /// in the derived SQLite table — we don't rename.
    pub field: String,
    /// SQLite affinity: `"text"` (default), `"integer"`, `"real"`.
    /// Storage maps directly via `sqlx::query.bind(...)`; type
    /// mismatches at INSERT-time fail loudly rather than silently
    /// dropping the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// When true, the projector creates a `CREATE INDEX` on this
    /// column at table-creation time. Boost for the common-filter
    /// columns (`name`, `version`) — operators mark them
    /// explicitly, the projector won't guess.
    #[serde(default)]
    pub index: bool,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct DisplayField {
    /// Top-level key in the stdout JSON.
    pub field: String,
    /// Human-readable column header.
    pub label: String,
    /// Optional render hint — `"number"`, `"bytes"`, `"timestamp"`,
    /// or `"table"` (#39). Defaults to plain text rendering on the
    /// SPA side. `"table"` expects the field's value to be a JSON
    /// array of objects and renders a nested sub-table on the
    /// per-PC detail page using `columns` as the schema; the fleet
    /// summary view falls back to showing the row count for
    /// `"table"` cells so the wide list stays compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// v0.30 / #39: when `kind == "table"`, the SPA renders the
    /// field's value (an array of objects like
    /// `disks: [{ device_id, size_bytes, ... }]`) as a nested
    /// sub-table using these columns. Each column is itself a
    /// `DisplayField`, so the nested cells reuse the same render
    /// hints (`bytes`, `number`, `timestamp`) — no parallel format
    /// pipeline. Ignored for any other `kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<DisplayField>>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct Rollout {
    #[serde(default)]
    pub strategy: RolloutStrategy,
    pub waves: Vec<Wave>,
}

#[derive(
    Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum RolloutStrategy {
    #[default]
    Wave,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct Wave {
    pub group: String,
    /// humantime delay measured from the deploy's publish time. wave[0]
    /// typically has "0s"; subsequent waves use minutes / hours.
    pub delay: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
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

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Execute {
    pub shell: ExecuteShell,
    /// Inline script body. Mutually exclusive with [`script_file`]
    /// and [`script_object`]; exactly one of the three must be set
    /// (enforced by [`Execute::validate_script_source`] at the
    /// write-side parse boundaries — `kanade job create` and
    /// `POST /api/jobs`).
    ///
    /// Empty string is treated as **unset** so operators can swap
    /// to a `script_file:` / `script_object:` alternative just by
    /// commenting out the body, without having to also drop the
    /// `script:` key entirely.
    ///
    /// [`script_file`]: Self::script_file
    /// [`script_object`]: Self::script_object
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Repo-local file path resolved by the operator-side CLI at
    /// `kanade job create` time. The CLI reads the file, slots its
    /// contents into `script`, and clears this field before
    /// POSTing — so the backend / agents never see `script_file`
    /// in stored manifests. SPEC §2.4.1.
    ///
    /// Resolver lands in a follow-up PR
    /// (yukimemi/kanade#210); today this field passes parse-time
    /// validation but the operator-side CLI bails with "not yet
    /// implemented" until the resolver ships, so manifests that
    /// reach the backend with `script_file` set are treated as a
    /// schema-bug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_file: Option<String>,
    /// Object Store reference (`<name>/<version>`) into the
    /// `scripts` bucket (`OBJECT_SCRIPTS`). Agents fetch the body
    /// at Execute time via `/api/script-objects/{name}/{version}`
    /// and cache it locally. SPEC §2.4.1.
    ///
    /// Resolver lands in the same follow-up PR as `script_file`;
    /// today this field passes parse-time validation but the
    /// backend / agent exec paths bail with "not yet implemented"
    /// when they see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_object: Option<String>,
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

impl Execute {
    /// Treat an empty `script:` body as "intentionally unset". Operators
    /// commenting out a block-scalar tend to leave the key behind, and
    /// failing the validator on `script: ""` would surprise them.
    fn has_inline_script(&self) -> bool {
        matches!(&self.script, Some(s) if !s.is_empty())
    }

    /// Enforce that exactly one of `script` / `script_file` /
    /// `script_object` is set. Called at the write-side parse
    /// boundaries (CLI `kanade job create` + backend
    /// `POST /api/jobs`) so ambiguous YAML is rejected before it
    /// reaches the JOBS KV. Read paths (projector, agent
    /// scheduler, list endpoints) skip this check — they only ever
    /// see what the write path already validated.
    pub fn validate_script_source(&self) -> Result<(), String> {
        let inline = self.has_inline_script();
        let file = self.script_file.is_some();
        let obj = self.script_object.is_some();
        let set = [inline, file, obj].into_iter().filter(|b| *b).count();
        match set {
            1 => Ok(()),
            0 => Err("execute: one of `script`, `script_file`, `script_object` must be set".into()),
            _ => Err(format!(
                "execute: only one of `script` / `script_file` / `script_object` may be set \
                 (got script={inline}, script_file={file}, script_object={obj})"
            )),
        }
    }
}

impl Manifest {
    /// Cross-field semantic checks that don't fit into pure serde
    /// derive. Currently delegates to
    /// [`Execute::validate_script_source`] — see that method's
    /// docs for the rationale on which call sites should run this.
    pub fn validate(&self) -> Result<(), String> {
        self.execute.validate_script_source()?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
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
            pcs: vec!["pc-01".into()],
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
        assert_eq!(
            m.execute.script.as_deref().map(str::trim),
            Some("echo 'kanade'")
        );
        assert!(m.execute.script_file.is_none());
        assert!(m.execute.script_object.is_none());
        assert_eq!(m.execute.timeout, "30s");
        assert!(!m.require_approval);
        m.validate()
            .expect("inline-script manifest passes validation");
    }

    fn execute_with(
        script: Option<&str>,
        script_file: Option<&str>,
        script_object: Option<&str>,
    ) -> Execute {
        Execute {
            shell: ExecuteShell::Powershell,
            script: script.map(str::to_owned),
            script_file: script_file.map(str::to_owned),
            script_object: script_object.map(str::to_owned),
            timeout: "30s".into(),
            run_as: RunAs::default(),
            cwd: None,
        }
    }

    #[test]
    fn validate_accepts_inline_script() {
        let e = execute_with(Some("echo hi"), None, None);
        assert!(e.validate_script_source().is_ok());
    }

    #[test]
    fn validate_accepts_script_file_alone() {
        let e = execute_with(None, Some("scripts/cleanup.ps1"), None);
        assert!(e.validate_script_source().is_ok());
    }

    #[test]
    fn validate_accepts_script_object_alone() {
        let e = execute_with(None, None, Some("cleanup/1.0.0"));
        assert!(e.validate_script_source().is_ok());
    }

    #[test]
    fn validate_treats_empty_inline_script_as_unset() {
        // `script: ""` + `script_object` set is the natural shape
        // when an operator comments out the YAML block-scalar body
        // but leaves the key. Should pass.
        let e = execute_with(Some(""), None, Some("cleanup/1.0.0"));
        assert!(e.validate_script_source().is_ok());
    }

    #[test]
    fn validate_rejects_zero_sources() {
        let e = execute_with(None, None, None);
        let err = e.validate_script_source().unwrap_err();
        assert!(err.contains("must be set"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_inline_only() {
        let e = execute_with(Some(""), None, None);
        let err = e.validate_script_source().unwrap_err();
        assert!(err.contains("must be set"), "got: {err}");
    }

    #[test]
    fn validate_rejects_inline_plus_file() {
        let e = execute_with(Some("echo hi"), Some("scripts/cleanup.ps1"), None);
        let err = e.validate_script_source().unwrap_err();
        assert!(err.contains("only one of"), "got: {err}");
    }

    #[test]
    fn validate_rejects_inline_plus_object() {
        let e = execute_with(Some("echo hi"), None, Some("cleanup/1.0.0"));
        let err = e.validate_script_source().unwrap_err();
        assert!(err.contains("only one of"), "got: {err}");
    }

    #[test]
    fn validate_rejects_file_plus_object() {
        let e = execute_with(None, Some("scripts/cleanup.ps1"), Some("cleanup/1.0.0"));
        let err = e.validate_script_source().unwrap_err();
        assert!(err.contains("only one of"), "got: {err}");
    }

    #[test]
    fn validate_rejects_all_three() {
        let e = execute_with(
            Some("echo hi"),
            Some("scripts/cleanup.ps1"),
            Some("cleanup/1.0.0"),
        );
        let err = e.validate_script_source().unwrap_err();
        assert!(err.contains("only one of"), "got: {err}");
    }

    #[test]
    fn manifest_deserialises_script_object_yaml() {
        // SPEC §2.4.1 example shape with the Object Store
        // reference picked over inline.
        let yaml = r#"
id: cleanup-disk-temp
version: 1.0.1
execute:
  shell: powershell
  script_object: cleanup-disk-temp/1.0.1
  timeout: 600s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(
            m.execute.script_object.as_deref(),
            Some("cleanup-disk-temp/1.0.1")
        );
        assert!(m.execute.script.is_none());
        m.validate()
            .expect("script_object-only manifest passes validation");
    }

    #[test]
    fn manifest_rejects_typo_in_script_field_name() {
        // `deny_unknown_fields` on Execute catches `script_objectt`
        // and similar fat-fingers at parse time instead of letting
        // them silently fall through to "all three unset".
        let yaml = r#"
id: typo
version: 1.0.0
execute:
  shell: powershell
  script_objectt: oops
  timeout: 30s
"#;
        let r: Result<Manifest, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err(), "expected parse error, got {r:?}");
    }

    #[test]
    fn schedule_carries_target_and_rollout() {
        let yaml = r#"
id: hourly-cleanup-canary
when:
  per_pc: { every: 1h }
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
id: kitting
when:
  per_pc: once
enabled: true
job_id: scheduled-echo
target: { all: true }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.id, "kitting");
        assert_eq!(s.when, When::PerPc(PerPolicy::Once(OnceLiteral::Once)));
        assert!(s.enabled);
        assert_eq!(s.job_id, "scheduled-echo");
        assert!(s.plan.target.all);
        assert!(s.plan.rollout.is_none());
        assert!(s.plan.jitter.is_none());
        assert!(s.active.is_empty());
    }

    #[test]
    fn schedule_enabled_defaults_to_true() {
        let yaml = r#"
id: x
when:
  per_pc: once
job_id: y
target: { all: true }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert!(s.enabled);
    }

    // ---- `when` parsing (#418 Phase 1) ----

    fn schedule_yaml_with(when_block: &str) -> String {
        format!(
            r#"
id: x
when:
{when_block}
job_id: y
target: {{ all: true }}
"#
        )
    }

    #[test]
    fn when_per_pc_every_parses_unquoted_humantime() {
        // `6h` is digit-led but non-numeric → YAML string, same as
        // the old `cooldown: 6h` convention. No quotes needed.
        let s: Schedule =
            serde_yaml::from_str(&schedule_yaml_with("  per_pc: { every: 6h }")).expect("parse");
        assert_eq!(
            s.when,
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() }))
        );
    }

    #[test]
    fn when_per_target_every_parses() {
        let s: Schedule = serde_yaml::from_str(&schedule_yaml_with("  per_target: { every: 24h }"))
            .expect("parse");
        assert_eq!(
            s.when,
            When::PerTarget(PerPolicy::Every(EverySpec {
                every: "24h".into()
            }))
        );
    }

    #[test]
    fn when_per_target_once_parses() {
        // Falls out of the shared PerPolicy shape and decide_fire
        // already implements it ("any one pc succeeds → skip the
        // target forever"), so it is allowed, not rejected.
        let s: Schedule =
            serde_yaml::from_str(&schedule_yaml_with("  per_target: once")).expect("parse");
        assert_eq!(s.when, When::PerTarget(PerPolicy::Once(OnceLiteral::Once)));
    }

    #[test]
    fn when_calendar_time_parses() {
        let s: Schedule = serde_yaml::from_str(&schedule_yaml_with(
            "  calendar:\n    at: \"09:00\"\n    days: [mon-fri]",
        ))
        .expect("parse");
        match &s.when {
            When::Calendar(c) => {
                assert_eq!(c.at, "09:00");
                assert_eq!(c.days, vec!["mon-fri"]);
            }
            other => panic!("expected calendar, got {other:?}"),
        }
    }

    #[test]
    fn when_calendar_days_default_empty() {
        let s: Schedule =
            serde_yaml::from_str(&schedule_yaml_with("  calendar:\n    at: \"09:00\""))
                .expect("parse");
        match &s.when {
            When::Calendar(c) => assert!(c.days.is_empty(), "days defaults to empty (= daily)"),
            other => panic!("expected calendar, got {other:?}"),
        }
    }

    #[test]
    fn when_calendar_datetime_parses_all_separators() {
        // one-shot: date+time in hyphen / ISO-T / slash forms
        for at in ["2026-06-10 09:00", "2026-06-10T09:00", "2026/06/10 09:00"] {
            let block = format!("  calendar:\n    at: \"{at}\"");
            let s: Schedule = serde_yaml::from_str(&schedule_yaml_with(&block))
                .unwrap_or_else(|e| panic!("parse '{at}': {e}"));
            match &s.when {
                When::Calendar(c) => {
                    use chrono::Datelike;
                    let p = c.parse_at().expect("parse_at");
                    let d = p.date.expect("datetime at carries a date");
                    assert_eq!((d.year(), d.month(), d.day()), (2026, 6, 10), "for '{at}'");
                }
                other => panic!("expected calendar, got {other:?}"),
            }
        }
    }

    #[test]
    fn when_rejects_bad_once_keyword() {
        // `onec` must be a parse error, not a silently-absorbed
        // string (OnceLiteral is a single-variant enum for exactly
        // this reason).
        let r: Result<Schedule, _> = serde_yaml::from_str(&schedule_yaml_with("  per_pc: onec"));
        assert!(r.is_err(), "expected parse error, got {r:?}");
    }

    #[test]
    fn when_rejects_unknown_key_in_every() {
        // EverySpec is deny_unknown_fields so `evry:` typos fail
        // even under the untagged PerPolicy.
        let r: Result<Schedule, _> =
            serde_yaml::from_str(&schedule_yaml_with("  per_pc: { evry: 6h }"));
        assert!(r.is_err(), "expected parse error, got {r:?}");
    }

    #[test]
    fn when_rejects_unknown_variant() {
        let r: Result<Schedule, _> =
            serde_yaml::from_str(&schedule_yaml_with("  per_galaxy: once"));
        assert!(r.is_err(), "expected parse error, got {r:?}");
    }

    #[test]
    fn when_rejects_old_top_level_cron_field() {
        // Pre-#418 shape: top-level `cron:` + no `when:`. Must fail
        // loudly (missing `when`), which is what turns stale KV
        // blobs into warn-skips after the upgrade.
        let yaml = r#"
id: x
cron: "* * * * * *"
job_id: y
target: { all: true }
"#;
        let r: Result<Schedule, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err(), "expected parse error, got {r:?}");
    }

    #[test]
    fn when_rejects_retired_cron_escape_hatch() {
        // #418 Phase 2 retired `when: { cron: "..." }`. A raw cron
        // is now an unknown variant → parse error (operators use the
        // calendar form instead).
        let r: Result<Schedule, _> =
            serde_yaml::from_str(&schedule_yaml_with("  cron: \"0 0 9 * * mon-fri\""));
        assert!(
            r.is_err(),
            "expected parse error for retired cron, got {r:?}"
        );
    }

    #[test]
    fn when_round_trips_json_and_yaml() {
        // Round-trip through the full Schedule: that is the wire
        // unit for both stores (JSON catalog KV + YAML mirror), and
        // it exercises the singleton_map field attribute that keeps
        // serde_yaml on the map shape instead of `!per_pc` tags.
        for when in [
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            When::PerTarget(PerPolicy::Once(OnceLiteral::Once)),
            When::PerTarget(PerPolicy::Every(EverySpec {
                every: "24h".into(),
            })),
            calendar("09:00", &["mon-fri"]),
            calendar("2026-06-10 09:00", &[]),
        ] {
            let s = schedule_with(when.clone(), RunsOn::Backend);

            let json = serde_json::to_string(&s).expect("json serialise");
            let back: Schedule = serde_json::from_str(&json).expect("json deserialise");
            assert_eq!(back.when, when, "json round-trip for {when}");

            let yaml = serde_yaml::to_string(&s).expect("yaml serialise");
            assert!(
                !yaml.contains('!'),
                "yaml must use the map shape, not tags: {yaml}"
            );
            let back: Schedule = serde_yaml::from_str(&yaml).expect("yaml deserialise");
            assert_eq!(back.when, when, "yaml round-trip for {when}");
        }
    }

    #[test]
    fn when_once_serialises_as_bare_keyword() {
        // The wire shape operators see in the YAML mirror must stay
        // the ergonomic `per_pc: once`, not a one-variant map.
        let json = serde_json::to_value(When::PerPc(PerPolicy::Once(OnceLiteral::Once)))
            .expect("serialise");
        assert_eq!(json, serde_json::json!({ "per_pc": "once" }));
    }

    #[test]
    fn when_displays_operator_summary() {
        for (when, expected) in [
            (
                When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
                "per_pc once",
            ),
            (
                When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
                "per_pc every 6h",
            ),
            (
                When::PerTarget(PerPolicy::Every(EverySpec {
                    every: "24h".into(),
                })),
                "per_target every 24h",
            ),
            (calendar("09:00", &["mon-fri"]), "at 09:00 [mon-fri]"),
            (calendar("2026-06-10 09:00", &[]), "at 2026-06-10 09:00"),
        ] {
            assert_eq!(when.to_string(), expected);
        }
    }

    // ---- lowering (#418: when → engine vocabulary) ----

    fn schedule_with(when: When, runs_on: RunsOn) -> Schedule {
        Schedule {
            id: "x".into(),
            when,
            job_id: "y".into(),
            plan: FanoutPlan::default(),
            active: Active::default(),
            tz: ScheduleTz::default(),
            starting_deadline: None,
            runs_on,
            enabled: true,
        }
    }

    fn calendar(at: &str, days: &[&str]) -> When {
        When::Calendar(CalendarSpec {
            at: at.into(),
            days: days.iter().map(|d| (*d).to_string()).collect(),
        })
    }

    #[test]
    fn lowering_matches_the_418_table() {
        let cases = [
            (
                When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
                (POLL_CRON, ExecMode::OncePerPc, None),
            ),
            (
                When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
                (POLL_CRON, ExecMode::OncePerPc, Some("6h")),
            ),
            (
                When::PerTarget(PerPolicy::Once(OnceLiteral::Once)),
                (POLL_CRON, ExecMode::OncePerTarget, None),
            ),
            (
                When::PerTarget(PerPolicy::Every(EverySpec {
                    every: "24h".into(),
                })),
                (POLL_CRON, ExecMode::OncePerTarget, Some("24h")),
            ),
            // calendar repeating → 6-field cron
            (
                calendar("09:00", &["mon-fri"]),
                ("0 0 9 * * mon-fri", ExecMode::EveryTick, None),
            ),
            // calendar daily (no days) → DOW *
            (
                calendar("18:30", &[]),
                ("0 30 18 * * *", ExecMode::EveryTick, None),
            ),
            // calendar one-shot → 7-field year cron
            (
                calendar("2026-06-10 09:00", &[]),
                ("0 0 9 10 6 * 2026", ExecMode::EveryTick, None),
            ),
        ];
        for (when, (cron, mode, cooldown)) in cases {
            let l = schedule_with(when.clone(), RunsOn::Backend).lowered();
            assert_eq!(l.cron, cron, "cron for {when}");
            assert_eq!(l.mode, mode, "mode for {when}");
            assert_eq!(l.cooldown.as_deref(), cooldown, "cooldown for {when}");
        }
    }

    #[test]
    fn lowered_carries_schedule_tz() {
        for (tz, want) in [
            (ScheduleTz::Local, ScheduleTz::Local),
            (ScheduleTz::Utc, ScheduleTz::Utc),
        ] {
            let mut s = schedule_with(calendar("09:00", &["mon-fri"]), RunsOn::Backend);
            s.tz = tz;
            assert_eq!(s.lowered().tz, want, "calendar carries tz");
            // reconcile shapes carry tz too (for the active-window check)
            let mut s = schedule_with(
                When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
                RunsOn::Backend,
            );
            s.tz = tz;
            assert_eq!(s.lowered().tz, want, "reconcile carries tz");
        }
    }

    #[test]
    fn poll_cron_is_accepted_by_the_engine_parser() {
        // POLL_CRON is system-generated — if the engine's parser
        // ever rejected it every reconcile schedule would die at
        // register time. Validate it with the same croner config
        // (Seconds::Required, dom_and_dow, year optional).
        croner::parser::CronParser::builder()
            .seconds(croner::parser::Seconds::Required)
            .dom_and_dow(true)
            .build()
            .parse(POLL_CRON)
            .expect("POLL_CRON must parse");
    }

    // ---- Schedule::validate() (#418 decision F) ----

    #[test]
    fn validate_accepts_reconcile_shapes() {
        for when in [
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            When::PerTarget(PerPolicy::Once(OnceLiteral::Once)),
            When::PerTarget(PerPolicy::Every(EverySpec {
                every: "24h".into(),
            })),
        ] {
            schedule_with(when.clone(), RunsOn::Backend)
                .validate()
                .unwrap_or_else(|e| panic!("{when} should validate: {e}"));
        }
    }

    #[test]
    fn validate_accepts_per_pc_on_agent() {
        schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "1h".into() })),
            RunsOn::Agent,
        )
        .validate()
        .expect("per_pc + agent is the offline-inventory shape");
    }

    #[test]
    fn validate_rejects_per_target_on_agent() {
        let err = schedule_with(
            When::PerTarget(PerPolicy::Every(EverySpec {
                every: "24h".into(),
            })),
            RunsOn::Agent,
        )
        .validate()
        .unwrap_err();
        assert!(err.contains("per_target"), "got: {err}");
        assert!(err.contains("runs_on: agent"), "got: {err}");

        // per_target: once is also backend-only.
        let err = schedule_with(
            When::PerTarget(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Agent,
        )
        .validate()
        .unwrap_err();
        assert!(err.contains("per_target"), "got (once): {err}");
        assert!(err.contains("runs_on: agent"), "got (once): {err}");
    }

    #[test]
    fn validate_rejects_bad_every_duration() {
        let err = schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "6x".into() })),
            RunsOn::Backend,
        )
        .validate()
        .unwrap_err();
        assert!(err.contains("when.every"), "got: {err}");
    }

    #[test]
    fn validate_rejects_bad_jitter_and_starting_deadline() {
        let mut s = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        s.plan.jitter = Some("5x".into());
        let err = s.validate().unwrap_err();
        assert!(err.contains("jitter"), "got: {err}");

        let mut s = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        s.starting_deadline = Some("soon".into());
        let err = s.validate().unwrap_err();
        assert!(err.contains("starting_deadline"), "got: {err}");
    }

    #[test]
    fn validate_accepts_calendar_shapes() {
        for when in [
            calendar("09:00", &["mon-fri"]),   // weekday morning
            calendar("00:00", &["sun"]),       // weekly
            calendar("18:30", &[]),            // daily
            calendar("2026-06-10 09:00", &[]), // one-shot
            calendar("2026/12/25 00:00", &[]), // one-shot, slash form
        ] {
            schedule_with(when.clone(), RunsOn::Backend)
                .validate()
                .unwrap_or_else(|e| panic!("{when} should validate: {e}"));
        }
    }

    #[test]
    fn validate_rejects_bad_at() {
        for bad in ["25:00", "09:60", "9", "noon", "2026-13-01 09:00"] {
            let err = schedule_with(calendar(bad, &[]), RunsOn::Backend)
                .validate()
                .unwrap_err();
            assert!(err.contains("when.at"), "for '{bad}', got: {err}");
        }
    }

    #[test]
    fn validate_rejects_datetime_at_with_days() {
        // A dated `at` is a one-shot — pairing it with days is a
        // contradiction (the date already pins the day).
        let err = schedule_with(calendar("2026-06-10 09:00", &["mon"]), RunsOn::Backend)
            .validate()
            .unwrap_err();
        assert!(
            err.contains("one-shot") && err.contains("days"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_rejects_bad_day_name() {
        // A garbage DOW token is caught by the days pre-flight and
        // reported against `when.days`, not the confusing
        // "when.at lowered to invalid cron" (claude #432 review).
        let err = schedule_with(calendar("09:00", &["funday"]), RunsOn::Backend)
            .validate()
            .unwrap_err();
        assert!(err.contains("when.days"), "got: {err}");
        assert!(err.contains("funday"), "names the bad token: {err}");
        // valid names / ranges / numeric / * all pass
        for ok in [
            calendar("09:00", &["mon-fri"]),
            calendar("09:00", &["mon", "wed", "sun"]),
            calendar("09:00", &["1-5"]),
        ] {
            schedule_with(ok.clone(), RunsOn::Backend)
                .validate()
                .unwrap_or_else(|e| panic!("{ok} should validate: {e}"));
        }
    }

    #[test]
    fn calendar_oneshot_instant_detects_past() {
        use chrono::TimeZone;
        // a dated `at` resolves to an absolute instant…
        let c = CalendarSpec {
            at: "2024-01-01 09:00".into(),
            days: vec![],
        };
        let t = c
            .oneshot_instant(ScheduleTz::Utc)
            .expect("one-shot instant");
        assert_eq!(
            t,
            chrono::Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap()
        );
        assert!(t < chrono::Utc::now(), "2024 is in the past");
        // …while a repeating (time-only) calendar has no instant
        let rep = CalendarSpec {
            at: "09:00".into(),
            days: vec!["mon-fri".into()],
        };
        assert!(rep.oneshot_instant(ScheduleTz::Utc).is_none());
    }

    fn schedule_with_active(from: Option<&str>, until: Option<&str>) -> Schedule {
        let mut s = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        s.active = Active {
            from: from.map(str::to_owned),
            until: until.map(str::to_owned),
        };
        s
    }

    #[test]
    fn validate_accepts_active_window() {
        schedule_with_active(Some("2026-07-01"), Some("2026-08-01T12:00:00+09:00"))
            .validate()
            .expect("date + rfc3339 bounds should validate");
    }

    #[test]
    fn validate_rejects_unparseable_active_bound() {
        let err = schedule_with_active(Some("July 1st"), None)
            .validate()
            .unwrap_err();
        assert!(err.contains("active"), "got: {err}");
    }

    #[test]
    fn validate_rejects_from_not_before_until() {
        let err = schedule_with_active(Some("2026-08-01"), Some("2026-07-01"))
            .validate()
            .unwrap_err();
        assert!(err.contains("strictly before"), "got: {err}");

        let err = schedule_with_active(Some("2026-07-01"), Some("2026-07-01"))
            .validate()
            .unwrap_err();
        assert!(err.contains("strictly before"), "got: {err}");
    }

    // ---- Active window semantics ----

    #[test]
    fn active_window_is_half_open() {
        use chrono::TimeZone;
        let active = Active {
            from: Some("2026-07-01".into()),
            until: Some("2026-08-01".into()),
        };
        // UTC tz so the date bounds are UTC midnight.
        let at = |y, m, d, h| chrono::Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        let c = |t| active.contains(t, ScheduleTz::Utc);
        assert!(!c(at(2026, 6, 30, 23)), "before from");
        assert!(c(at(2026, 7, 1, 0)), "at from (inclusive)");
        assert!(c(at(2026, 7, 15, 12)), "inside");
        assert!(!c(at(2026, 8, 1, 0)), "at until (exclusive)");
        assert!(!c(at(2026, 8, 2, 0)), "after until");
    }

    #[test]
    fn active_empty_window_is_always_active() {
        assert!(Active::default().contains(chrono::Utc::now(), ScheduleTz::Local));
    }

    #[test]
    fn active_rfc3339_bound_honours_offset_regardless_of_tz() {
        use chrono::TimeZone;
        let active = Active {
            from: Some("2026-07-01T09:00:00+09:00".into()),
            until: None,
        };
        // RFC3339 carries its own offset → tz arg is ignored.
        // 09:00 JST = 00:00 UTC.
        for tz in [ScheduleTz::Utc, ScheduleTz::Local] {
            assert!(
                !active.contains(
                    chrono::Utc
                        .with_ymd_and_hms(2026, 6, 30, 23, 59, 0)
                        .unwrap(),
                    tz
                )
            );
            assert!(active.contains(
                chrono::Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
                tz
            ));
        }
    }

    #[test]
    fn active_date_bound_respects_tz() {
        // A bare `YYYY-MM-DD` bound is midnight *in the schedule's
        // tz* (#418 Phase 2). The UTC interpretation is exact and
        // host-independent; assert that precisely.
        use chrono::TimeZone;
        let utc = Active::parse_bound("2026-07-01", ScheduleTz::Utc).expect("utc");
        assert_eq!(
            utc,
            chrono::Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()
        );

        // The local interpretation must equal what chrono::Local
        // computes for the same wall-clock midnight — proves the tz
        // path is wired to the host zone (the magnitude vs UTC is
        // host-dependent, so we compare against Local directly rather
        // than hard-coding the JST offset, keeping CI green on UTC
        // runners).
        let local = Active::parse_bound("2026-07-01", ScheduleTz::Local).expect("local");
        let want = chrono::Local
            .with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
            .single()
            .expect("local midnight is unambiguous")
            .with_timezone(&chrono::Utc);
        assert_eq!(local, want, "date bound resolved in host-local tz");
    }

    #[test]
    fn active_empty_is_skipped_when_serialising() {
        let s = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        let json = serde_json::to_value(&s).expect("serialise");
        assert!(
            json.get("active").is_none(),
            "empty active must not appear on the wire: {json}"
        );
    }

    #[test]
    fn shipped_schedule_configs_parse_and_validate() {
        // Every YAML under configs/schedules/ must parse with the
        // current Schedule serde AND pass validate() — keeps the
        // shipped examples from drifting out of sync with the model
        // (#418 removed back-compat, so drift = broken at create).
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/schedules");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).expect("read configs/schedules") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("read yaml");
            let s: Schedule = serde_yaml::from_str(&body)
                .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
            s.validate()
                .unwrap_or_else(|e| panic!("{} failed validate(): {e}", path.display()));
            seen += 1;
        }
        assert!(seen > 0, "no schedule YAMLs found in {}", dir.display());
    }

    // ---- pre-existing enum wire formats (unchanged by #418) ----

    #[test]
    fn exec_mode_serialises_snake_case() {
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
    fn schedule_runs_on_defaults_to_backend() {
        let yaml = r#"
id: x
when:
  per_pc: once
job_id: y
target: { all: true }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.runs_on, RunsOn::Backend);
    }

    #[test]
    fn schedule_runs_on_agent_parses() {
        let yaml = r#"
id: offline-inv
when:
  per_pc: { every: 1h }
job_id: inventory-hw
target: { all: true }
runs_on: agent
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.runs_on, RunsOn::Agent);
        assert_eq!(s.lowered().mode, ExecMode::OncePerPc);
    }

    #[test]
    fn runs_on_serialises_snake_case() {
        for (mode, expected) in [(RunsOn::Backend, "backend"), (RunsOn::Agent, "agent")] {
            let s = serde_json::to_value(mode).expect("serialise");
            assert_eq!(s, serde_json::Value::String(expected.into()));
            let back: RunsOn = serde_json::from_value(serde_json::Value::String(expected.into()))
                .expect("deserialise");
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn execute_shell_into_wire_shell() {
        assert_eq!(Shell::from(ExecuteShell::Powershell), Shell::Powershell);
        assert_eq!(Shell::from(ExecuteShell::Cmd), Shell::Cmd);
    }

    #[test]
    fn manifest_staleness_defaults_to_cached() {
        let yaml = r#"
id: x
version: 1.0.0
execute:
  shell: powershell
  script: "echo"
  timeout: 1s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(m.staleness, Staleness::Cached);
    }

    #[test]
    fn manifest_strict_staleness_parses() {
        let yaml = r#"
id: urgent-patch
version: 2.5.1
execute:
  shell: powershell
  script: Install-Hotfix
  timeout: 5m
staleness:
  mode: strict
  max_cache_age: 0s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        match m.staleness {
            Staleness::Strict { max_cache_age } => assert_eq!(max_cache_age, "0s"),
            other => panic!("expected strict, got {other:?}"),
        }
    }

    #[test]
    fn manifest_unchecked_staleness_parses() {
        let yaml = r#"
id: legacy
version: 0.1.0
execute:
  shell: cmd
  script: "echo"
  timeout: 1s
staleness:
  mode: unchecked
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(m.staleness, Staleness::Unchecked);
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

    #[test]
    fn display_field_table_kind_round_trips_with_nested_columns() {
        // #39: `type: table` + `columns:` on a DisplayField gets
        // round-tripped through serde so the SPA receives the
        // nested schema verbatim. Nested columns themselves are
        // DisplayFields so they can carry `type: bytes` /
        // `type: number` for cell formatting.
        let yaml = r#"
id: inv-hw
version: 1.0.0
execute:
  shell: powershell
  script: "echo"
  timeout: 60s
inventory:
  display:
    - field: hostname
      label: Hostname
    - field: disks
      label: Disks
      type: table
      columns:
        - field: device_id
          label: Drive
        - field: size_bytes
          label: Size
          type: bytes
        - field: free_bytes
          label: Free
          type: bytes
        - field: file_system
          label: FS
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let inv = m.inventory.as_ref().expect("inventory hint");
        let disks = inv
            .display
            .iter()
            .find(|d| d.field == "disks")
            .expect("disks display row");
        assert_eq!(disks.kind.as_deref(), Some("table"));
        let cols = disks.columns.as_ref().expect("table needs columns");
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[1].field, "size_bytes");
        assert_eq!(cols[1].kind.as_deref(), Some("bytes"));
    }

    #[test]
    fn display_field_scalar_kind_keeps_columns_none() {
        // Defensive: when type is a scalar (`bytes` / `number` /
        // `timestamp`) the `columns` field stays None — the SPA
        // uses its presence as the "render nested table" signal,
        // so it must not leak in via serde defaults.
        let yaml = r#"
id: x
version: 1.0.0
execute:
  shell: powershell
  script: "echo"
  timeout: 5s
inventory:
  display:
    - { field: ram_bytes, label: RAM, type: bytes }
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let inv = m.inventory.as_ref().unwrap();
        assert!(inv.display[0].columns.is_none());
    }
}

/// Periodic schedule (spec §2.4.3). v0.18.0 carries the fanout plan
/// (target + optional rollout + optional jitter) inline; the
/// referenced job (`job_id` → [`BUCKET_JOBS`]) supplies only the
/// script body. Two schedules of the same job can target different
/// groups on different cadences without copying the manifest.
///
/// #418 Phase 1: the cadence is the single [`When`] field. The old
/// `cron` × `mode` × `cooldown` × `auto_disable_when_done` quartet
/// is gone (no back-compat — pre-Phase-1 KV blobs fail to parse and
/// are warn-skipped; re-`schedule create` to upgrade them). The
/// engine underneath is unchanged: [`Schedule::lowered`] maps `when`
/// onto the same (cron, ExecMode, cooldown) trio the scheduler and
/// `decide_fire` always ran on.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct Schedule {
    pub id: String,
    /// When to fire — a reconcile cadence (`per_pc` / `per_target`)
    /// or a calendar time trigger (`at` / `days`). See [`When`].
    ///
    /// `singleton_map`: serde_yaml 0.9 renders externally-tagged
    /// enums as `!per_pc` YAML tags by default; this keeps the
    /// operator-facing map shape (`when: { per_pc: once }`). JSON
    /// output is identical either way, and the schemars schema
    /// (external tagging = oneOf of single-key objects) already
    /// matches the singleton-map wire shape.
    #[serde(with = "serde_yaml::with::singleton_map")]
    #[schemars(with = "When")]
    pub when: When,
    /// Key into [`crate::kv::BUCKET_JOBS`]. Must equal a registered
    /// Manifest's `id`.
    pub job_id: String,
    /// Who + how-to-phase + when-to-stagger. The Manifest doesn't
    /// carry these any more — same job + different fanout = different
    /// schedule.
    #[serde(flatten)]
    pub plan: FanoutPlan,
    /// Optional validity window. Outside `[from, until)` the
    /// schedule is dormant — still registered, still visible, but
    /// every tick is skipped (deleted ≠ dormant: a campaign that
    /// ended stays inspectable and can be re-armed by editing the
    /// window). Checked at tick time on both the backend scheduler
    /// and the agent's local scheduler.
    #[serde(default, skip_serializing_if = "Active::is_empty")]
    pub active: Active,
    /// #418 Phase 2: the timezone this schedule's wall-clock fields
    /// are evaluated in — both the calendar `at` firing time AND the
    /// `active.{from,until}` window bounds. `local` (default) = the
    /// running host's TZ (the agent's for `runs_on: agent`, the
    /// backend server's otherwise); `utc` for TZ-independent
    /// schedules. Reconcile shapes (`per_pc`/`per_target`) ignore it
    /// for firing (poll cron runs every minute regardless) but still
    /// honor it for the `active` window.
    #[serde(default)]
    pub tz: ScheduleTz,
    /// v0.22: optional humantime window after a cron tick during
    /// which the Command is still considered "live". The scheduler
    /// computes `tick_at + starting_deadline` and stamps it onto
    /// each Command as `deadline_at`; agents skip Commands they
    /// receive after that absolute time. `None` (default) = no
    /// deadline, meaning a Command queued in the broker / stream
    /// during agent downtime runs whenever the agent reconnects —
    /// good for kitting / inventory / cleanup. Set this for
    /// time-of-day notifications, lunch reminders, etc., where
    /// "fire 3 hours late" would be wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_deadline: Option<String>,
    /// v0.23: where does the cron tick happen? `Backend` (default,
    /// historical) = backend's scheduler fires Commands via NATS;
    /// agents passively receive. `Agent` = each targeted agent runs
    /// its own internal cron and fires locally, so the schedule
    /// keeps ticking even when the broker is unreachable (laptop on
    /// the train, broker maintenance window, full WAN outage). The
    /// two locations are mutually exclusive — when `Agent`, the
    /// backend scheduler stays out and just keeps the definition in
    /// KV for agents to read.
    #[serde(default)]
    pub runs_on: RunsOn,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// v0.23 — where the cron tick fires from.
#[derive(
    Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum RunsOn {
    /// Backend's central scheduler ticks and publishes Commands to
    /// NATS. Historical default, what every pre-v0.23 schedule
    /// uses. Agent offline ⇒ Command queued in STREAM_EXEC; agent
    /// reconnects ⇒ catch-up via [`command_replay`](crate)
    /// (see kanade-agent's command_replay module).
    #[default]
    Backend,
    /// Each targeted agent runs the cron tick locally. Survives
    /// broker / WAN outages. Best for laptops / mobile devices that
    /// roam off the corporate network. Agent must be online for the
    /// initial schedule + job-catalog pull, but once cached the
    /// agent fires the script standalone.
    Agent,
}

/// Per-pc/per-target dedup semantics for a [`Schedule`] (v0.19).
#[derive(
    Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
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

/// #418 Phase 1 — the single "when does this fire" axis.
///
/// Replaces the old `cron` + `mode` + `cooldown` trio whose
/// interactions were implicit (cron doubled as both a real
/// time-of-day trigger and a reconcile poll period; contradictory
/// combinations silently no-opped). Two shapes:
///
/// * **reconcile** (`per_pc` / `per_target`) — desired-state: "each
///   pc (or one delegate) should have run this within `every`".
///   The poll period is system-generated ([`POLL_CRON`], every
///   minute) and no longer the operator's concern.
/// * **calendar** (`{ at, days }`) — a wall-clock time trigger
///   (#418 Phase 2, replacing the old raw-cron escape hatch). Fires
///   the whole target at the given time, no dedup. `at: "09:00"` +
///   `days` repeats; `at: "2026-06-10 09:00"` (a date+time) fires
///   exactly once. Evaluated in the schedule's top-level `tz`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// Fire at each targeted pc: `once` (kitting — succeed once,
    /// skip forever, forever catching brand-new / re-imaged pcs)
    /// or `{ every: <humantime> }` (patrol — re-arm per pc after
    /// the interval).
    PerPc(PerPolicy),
    /// Fire until **any** one pc of the target succeeds, then skip
    /// the whole target (`once`) or re-arm after `every`. Needs
    /// fleet-wide completion data, so it is backend-only —
    /// `runs_on: agent` + `per_target` is rejected by
    /// [`Schedule::validate`].
    PerTarget(PerPolicy),
    /// Calendar time trigger: `{ at: "09:00", days: [mon-fri] }`
    /// (repeating) or `{ at: "2026-06-10 09:00" }` (one-shot). Fires
    /// the whole target at that wall-clock time in the schedule's
    /// `tz` — no dedup, no cooldown.
    Calendar(CalendarSpec),
}

/// Calendar time trigger (#418 Phase 2). `at` is either a time of
/// day (`"HH:MM"`, repeating — combine with `days`) or a full
/// date+time (`"YYYY-MM-DD HH:MM"`, a one-shot that fires once and
/// never again). Evaluated in the schedule's top-level `tz`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CalendarSpec {
    /// `"HH:MM"` (24h) for a repeating trigger, or
    /// `"YYYY-MM-DD HH:MM"` (hyphen / slash / `T` separators all
    /// accepted) for a one-shot. Parsed lazily —
    /// [`Schedule::validate`] rejects garbage at create time.
    pub at: String,
    /// Day-of-week filter for a time-of-day `at`: `["mon-fri"]`,
    /// `["mon","wed","fri"]`, … (passed verbatim to the cron DOW
    /// field, so ranges and names both work). Empty = every day.
    /// Must be empty when `at` carries a date (the date already
    /// pins the day).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<String>,
}

/// Parsed `CalendarSpec.at`: the wall-clock minute/hour, plus the
/// date for a one-shot (`None` = repeating time-of-day).
struct ParsedAt {
    minute: u32,
    hour: u32,
    date: Option<chrono::NaiveDate>,
}

impl CalendarSpec {
    /// Parse `at`: a date+time (`YYYY-MM-DD HH:MM`, hyphen / slash /
    /// `T` separators) is a one-shot; a bare `HH:MM` is repeating.
    fn parse_at(&self) -> Result<ParsedAt, String> {
        use chrono::Timelike;
        let s = self.at.trim();
        for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y/%m/%d %H:%M"] {
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                return Ok(ParsedAt {
                    minute: dt.minute(),
                    hour: dt.hour(),
                    date: Some(dt.date()),
                });
            }
        }
        if let Ok(t) = chrono::NaiveTime::parse_from_str(s, "%H:%M") {
            return Ok(ParsedAt {
                minute: t.minute(),
                hour: t.hour(),
                date: None,
            });
        }
        Err(format!(
            "when.at: unparseable '{}' (want HH:MM or YYYY-MM-DD HH:MM)",
            self.at
        ))
    }

    /// Pre-flight check on the `days` tokens so a bad day name gives
    /// a `when.days:`-scoped error instead of croner's confusing
    /// "when.at lowered to invalid cron" (claude #432 review). Each
    /// token is a day name (`mon`..`sun`), a numeric DOW (`0`..`7`),
    /// `*`, or a `-` range of those.
    fn validate_days(&self) -> Result<(), String> {
        const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        for tok in &self.days {
            for part in tok.split('-') {
                let p = part.trim().to_ascii_lowercase();
                let ok = p == "*"
                    || NAMES.contains(&p.as_str())
                    || p.parse::<u8>().map(|n| n <= 7).unwrap_or(false);
                if !ok {
                    return Err(format!(
                        "when.days: invalid day token '{part}' \
                         (want mon..sun, 0-7, a range like mon-fri, or *)"
                    ));
                }
            }
        }
        Ok(())
    }

    /// For a one-shot (`at` carries a date), the absolute instant it
    /// fires in `tz`. `None` for a repeating calendar. Used to warn
    /// about a one-shot whose date is already in the past (it would
    /// never fire).
    pub fn oneshot_instant(&self, tz: ScheduleTz) -> Option<chrono::DateTime<chrono::Utc>> {
        let p = self.parse_at().ok()?;
        let date = p.date?;
        let naive = date.and_hms_opt(p.hour, p.minute, 0)?;
        tz.naive_to_utc(naive)
    }

    /// Lower to the cron string the scheduler engine runs. Repeating
    /// → 6-field `0 {min} {hour} * * {dow}`; one-shot → 7-field
    /// `0 {min} {hour} {day} {month} * {year}` (a past year never
    /// fires — that's what makes it one-shot).
    fn to_cron(&self) -> Result<String, String> {
        use chrono::Datelike;
        let ParsedAt { minute, hour, date } = self.parse_at()?;
        match date {
            Some(d) => {
                if !self.days.is_empty() {
                    return Err(
                        "when.at with a date is a one-shot and cannot be combined with days".into(),
                    );
                }
                Ok(format!(
                    "0 {minute} {hour} {} {} * {}",
                    d.day(),
                    d.month(),
                    d.year()
                ))
            }
            None => {
                let dow = if self.days.is_empty() {
                    "*".to_string()
                } else {
                    self.validate_days()?;
                    self.days.join(",")
                };
                Ok(format!("0 {minute} {hour} * * {dow}"))
            }
        }
    }
}

/// The timezone a schedule's wall-clock fields (`when.at`,
/// `active.{from,until}`) are evaluated in (#418 Phase 2).
#[derive(
    Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleTz {
    /// The running host's local timezone — the agent's for
    /// `runs_on: agent`, the backend server's otherwise. Default.
    #[default]
    Local,
    /// UTC — for timezone-independent schedules.
    Utc,
}

impl ScheduleTz {
    /// Interpret a naive (zoneless) datetime as being in this tz and
    /// convert to UTC. On a DST *fold* (the local time occurs twice
    /// when clocks go back) we pick `.earliest()` rather than
    /// rejecting it; `None` is reserved for a true DST *gap* (a local
    /// time that never exists). `Utc` is fixed-offset so neither ever
    /// happens; `Local` is whatever timezone the running host is set
    /// to and *can* hit a gap/fold on any DST-observing host — not
    /// just the JST we run today (gemini + claude #432 review).
    fn naive_to_utc(self, naive: chrono::NaiveDateTime) -> Option<chrono::DateTime<chrono::Utc>> {
        use chrono::TimeZone;
        match self {
            ScheduleTz::Utc => Some(chrono::DateTime::from_naive_utc_and_offset(
                naive,
                chrono::Utc,
            )),
            ScheduleTz::Local => chrono::Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.with_timezone(&chrono::Utc)),
        }
    }
}

/// `once` vs `{ every: <humantime> }` — shared by `per_pc` /
/// `per_target`. Untagged so the YAML stays the bare keyword or a
/// one-key map, nothing more ceremonial.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum PerPolicy {
    /// The bare string `once`: succeed once, then skip permanently
    /// (cooldown = infinity).
    Once(OnceLiteral),
    /// Re-arm after the humantime interval, e.g. `{ every: 6h }`.
    Every(EverySpec),
}

/// Single-variant enum so serde accepts exactly the string `once`
/// (a free-form `String` would swallow typos like `onec`).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnceLiteral {
    Once,
}

/// `{ every: <humantime> }`. Standalone struct (not an inline
/// struct variant) so `deny_unknown_fields` still bites under the
/// untagged [`PerPolicy`] — `{ evry: 6h }` is a parse error, not a
/// silently-ignored key.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EverySpec {
    /// Humantime interval (`10m`, `6h`, `1d`...). Parsed lazily —
    /// [`Schedule::validate`] rejects garbage at create time.
    pub every: String,
}

impl PerPolicy {
    /// The cooldown this policy lowers to: `once` = `None`
    /// (permanent skip), `every` = the interval.
    fn cooldown(&self) -> Option<String> {
        match self {
            PerPolicy::Once(_) => None,
            PerPolicy::Every(EverySpec { every }) => Some(every.clone()),
        }
    }
}

impl std::fmt::Display for When {
    /// Operator-facing one-liner (`per_pc once` / `per_pc every 6h`
    /// / `at 09:00 [mon-fri]` / `at 2026-06-10 09:00`) for log
    /// lines, audit payloads and the API's `ScheduleSummary`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let policy = |p: &PerPolicy| match p {
            PerPolicy::Once(_) => "once".to_string(),
            PerPolicy::Every(EverySpec { every }) => format!("every {every}"),
        };
        match self {
            When::PerPc(p) => write!(f, "per_pc {}", policy(p)),
            When::PerTarget(p) => write!(f, "per_target {}", policy(p)),
            When::Calendar(c) if c.days.is_empty() => write!(f, "at {}", c.at),
            When::Calendar(c) => write!(f, "at {} [{}]", c.at, c.days.join(",")),
        }
    }
}

/// Optional validity window for a [`Schedule`] (#418 decision G).
/// Half-open `[from, until)`; either bound may be omitted. Bounds
/// are `YYYY-MM-DD` (= that day's 00:00 in the schedule's `tz`) or
/// full RFC3339 (offset is honored as-is, `tz` ignored). Kept as
/// strings so the JSON Schema the SPA editor consumes stays two
/// plain string fields, mirroring `jitter` / `starting_deadline`.
///
/// #418 Phase 2: bounds are evaluated in the schedule's top-level
/// `tz` (was UTC-only in Phase 1) so `tz: local` makes both the
/// calendar `at` AND the `active` window local — one consistent
/// timezone per schedule.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Active {
    /// Dormant before this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Dormant from this instant on (exclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

impl Active {
    /// `skip_serializing_if` helper — an empty window means "always
    /// active" and is omitted from the wire format entirely.
    pub fn is_empty(&self) -> bool {
        self.from.is_none() && self.until.is_none()
    }

    /// Parse one bound: RFC3339 first (offset honored, `tz`
    /// ignored), then bare `YYYY-MM-DD` (00:00 in `tz`).
    pub fn parse_bound(s: &str, tz: ScheduleTz) -> Result<chrono::DateTime<chrono::Utc>, String> {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return Ok(dt.with_timezone(&chrono::Utc));
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            let midnight = d.and_hms_opt(0, 0, 0).expect("00:00:00 is always valid");
            return tz.naive_to_utc(midnight).ok_or_else(|| {
                format!("active: bound '{s}' falls in a DST gap for the schedule's tz")
            });
        }
        Err(format!(
            "active: unparseable bound '{s}' (want YYYY-MM-DD or RFC3339)"
        ))
    }

    /// Is `now` inside the window? Unparseable bounds are treated
    /// as absent here (fail-open) — [`Schedule::validate`] is the
    /// place that rejects them loudly; this runs on every tick and
    /// must never panic on a stale KV blob.
    pub fn contains(&self, now: chrono::DateTime<chrono::Utc>, tz: ScheduleTz) -> bool {
        let bound = |s: &Option<String>| s.as_deref().and_then(|s| Self::parse_bound(s, tz).ok());
        if bound(&self.from).is_some_and(|from| now < from) {
            return false;
        }
        if bound(&self.until).is_some_and(|until| now >= until) {
            return false;
        }
        true
    }
}

/// The system-generated poll cadence every reconcile-shaped `when`
/// lowers to. Operators never write this: the real inter-run
/// spacing is the `every` cooldown; this only bounds "how soon do
/// we notice somebody is due" (#418 decision B took the poll
/// period away from the operator).
pub const POLL_CRON: &str = "0 * * * * *";

/// What a [`When`] lowers to — the exact (cron, mode, cooldown)
/// trio the pre-#418 engine ran on. Keeping the engine vocabulary
/// unchanged is what lets Phase 1 swap the operator surface without
/// touching the tick / dedup machinery.
pub struct Lowered {
    /// Cron handed to `tokio-cron-scheduler` — [`POLL_CRON`] for
    /// reconcile shapes, a 6/7-field cron for calendar shapes.
    pub cron: String,
    /// Dedup semantics for `decide_fire`.
    pub mode: ExecMode,
    /// Humantime re-arm interval (`None` = succeed once, skip
    /// forever).
    pub cooldown: Option<String>,
    /// Timezone to evaluate `cron` in (#418 Phase 2). The scheduler
    /// passes this to `Job::new_async_tz`. Reconcile shapes carry
    /// the schedule's tz too even though POLL_CRON is tz-agnostic,
    /// so the same value drives the `active`-window check.
    pub tz: ScheduleTz,
}

impl Schedule {
    /// Lower the operator-facing `when` onto the engine vocabulary.
    /// Single seam shared by the backend scheduler and the agent's
    /// local scheduler so the two can never drift.
    pub fn lowered(&self) -> Lowered {
        let tz = self.tz;
        match &self.when {
            When::PerPc(p) => Lowered {
                cron: POLL_CRON.into(),
                mode: ExecMode::OncePerPc,
                cooldown: p.cooldown(),
                tz,
            },
            When::PerTarget(p) => Lowered {
                cron: POLL_CRON.into(),
                mode: ExecMode::OncePerTarget,
                cooldown: p.cooldown(),
                tz,
            },
            // `to_cron` only fails on a malformed `at` (rejected by
            // validate() at create time). For a hand-edited KV blob
            // that slipped past, emit a deliberately-invalid cron so
            // register()'s Job::new_async_tz fails → warn+skip,
            // rather than firing at the wrong time.
            When::Calendar(c) => Lowered {
                cron: c
                    .to_cron()
                    .unwrap_or_else(|_| "# invalid calendar at".into()),
                mode: ExecMode::EveryTick,
                cooldown: None,
                tz,
            },
        }
    }

    /// Cross-field semantic checks that don't fit pure serde derive
    /// — the [`Manifest::validate`] counterpart (#418 decision F;
    /// pre-Phase-1 a broken schedule was accepted at create time
    /// and silently warn-skipped at tick time). Run at every create
    /// site: `kanade schedule create` (client-side) and
    /// `POST /api/schedules`. The job_id-exists check lives in the
    /// API handler instead — it needs the JOBS KV.
    pub fn validate(&self) -> Result<(), String> {
        if matches!(self.runs_on, RunsOn::Agent) && matches!(self.when, When::PerTarget(_)) {
            return Err(
                "when.per_target needs fleet-wide completion data and is backend-only; \
                 it cannot be combined with runs_on: agent (each agent self-schedules, \
                 so per-target dedup would be deduping across a target of 1)"
                    .into(),
            );
        }
        if let Some(cd) = self.lowered().cooldown.as_deref() {
            humantime::parse_duration(cd)
                .map_err(|e| format!("when.every: invalid duration '{cd}': {e}"))?;
        }
        if let When::Calendar(c) = &self.when {
            // Lower the calendar form to its cron (catches a bad `at`
            // and the date+days conflict), then validate that cron
            // with the same parser configuration tokio-cron-scheduler
            // 0.15 uses internally (croner, seconds required,
            // DOM-and-DOW both honored, year optional) — create-time
            // validation can never accept what register() rejects.
            let cron = c.to_cron()?;
            croner::parser::CronParser::builder()
                .seconds(croner::parser::Seconds::Required)
                .dom_and_dow(true)
                .build()
                .parse(&cron)
                .map_err(|e| format!("when.at lowered to invalid cron '{cron}': {e}"))?;
        }
        // The other humantime strings on the schedule (claude #419
        // review): runtime degrades gracefully on both (bad jitter →
        // silent no-op, bad starting_deadline → warn + skipped tick),
        // but "rejected at create time" should cover every field the
        // operator can typo, not just `when`.
        if let Some(j) = &self.plan.jitter {
            humantime::parse_duration(j)
                .map_err(|e| format!("jitter: invalid duration '{j}': {e}"))?;
        }
        if let Some(sd) = &self.starting_deadline {
            humantime::parse_duration(sd)
                .map_err(|e| format!("starting_deadline: invalid duration '{sd}': {e}"))?;
        }
        let from = self
            .active
            .from
            .as_deref()
            .map(|s| Active::parse_bound(s, self.tz))
            .transpose()?;
        let until = self
            .active
            .until
            .as_deref()
            .map(|s| Active::parse_bound(s, self.tz))
            .transpose()?;
        if let (Some(f), Some(u)) = (from, until) {
            if f >= u {
                return Err(format!(
                    "active.from ({}) must be strictly before active.until ({})",
                    self.active.from.as_deref().unwrap_or_default(),
                    self.active.until.as_deref().unwrap_or_default(),
                ));
            }
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}
