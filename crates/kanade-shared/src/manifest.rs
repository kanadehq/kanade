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
    fn schedule_runs_on_defaults_to_backend() {
        let yaml = r#"
id: x
cron: "* * * * * *"
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
cron: "0 0 * * * *"
job_id: inventory-hw
target: { all: true }
runs_on: agent
mode: once_per_pc
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.runs_on, RunsOn::Agent);
        assert_eq!(s.mode, ExecMode::OncePerPc);
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
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
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

fn default_true() -> bool {
    true
}
