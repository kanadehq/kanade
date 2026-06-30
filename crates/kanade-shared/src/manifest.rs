use serde::{Deserialize, Serialize};

use crate::wire::{FinalizeCommand, RunAs, Shell, Staleness};

/// YAML job manifest (= registered "what to run", v0.18.0+).
///
/// Owns only script-intrinsic fields. **Who** (`target`), **how to
/// phase fanout** (`rollout`), and **when to stagger start**
/// (`jitter`) all moved to the Schedule / exec request side — same
/// script can now be fired against different targets / rollouts
/// without copying the script body.
///
/// #492: these types are READ fleet-wide (agents decode them from
/// BUCKET_JOBS / BUCKET_SCHEDULES and inside live Commands), so they
/// must tolerate unknown fields — `deny_unknown_fields` here made a
/// gradually-upgrading fleet's OLD agents reject the whole object
/// the moment a newer backend added any field. Operator typo
/// protection (the old reason for the attribute) lives at the WRITE
/// boundaries instead: `kanade job/schedule create` and the backend
/// POST extractor parse via [`crate::strict`], which rejects unknown
/// keys with their full paths. The wire rule: new fields always get
/// `#[serde(default)]` (+ `skip_serializing_if` while old readers
/// may still be strict).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
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
    /// #290: opt-in marker that this job is an operator-defined
    /// **health check** whose result feeds the Client App's Health
    /// tab over KLP (`StateSnapshot.checks`). The script prints a
    /// free-form JSON object on stdout (like any inventory job); the
    /// agent reads the [`CheckHint::status_field`] value dynamically
    /// into a [`crate::ipc::state::Check`] named `check.name`.
    /// Cadence / windows / conditions come from
    /// the job's Schedule (exactly like inventory) — there is
    /// deliberately no interval here. **Composes with `inventory:` and
    /// `collect:`** (#821): each reads its own `#KANADE-<KIND>`-fenced
    /// stdout block, so one job can drive a check, project inventory
    /// facts, and collect files in a single run. Only `emit:` (NDJSON
    /// stdout) is incompatible. A check-only job may skip the fence
    /// (whole stdout is the JSON); a multi-hint job fences each block.
    #[serde(default)]
    pub check: Option<CheckHint>,
    /// #219: opt-in marker that this job COLLECTS files into a bundle.
    /// The script does the collection work and prints a single JSON
    /// object on stdout carrying a `files` array of paths (the field
    /// name is [`CollectHint::files_field`], default `"files"`); the
    /// agent — after the script exits successfully — zips those files,
    /// uploads the archive to the `OBJECT_COLLECTIONS` Object Store
    /// bucket (key `<pc_id>/<job_id>/<timestamp>.zip`), and records the
    /// key in [`crate::wire::ExecResult::collect_object`]. The operator
    /// downloads bundles from the SPA Collect page.
    ///
    /// Like `inventory:` / `check:` this reads a JSON object from stdout.
    /// #821: it reads its own `#KANADE-COLLECT-BEGIN/END`-fenced block,
    /// so it **composes with `inventory:` / `check:`** (and a user
    /// message) on one stdout — only `emit:` (NDJSON) is incompatible
    /// (enforced in [`Manifest::validate`]). A collect-only job may skip
    /// the fence. It also composes with `client:` — a `collect:` +
    /// `client:` job lets an end user trigger a collection from the
    /// Client App (the same-host agent runs it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect: Option<CollectHint>,
    /// #720: opt-in declarative aggregation over `obs_events` that drives
    /// the SPA **Analytics** page. Unlike the other hints this one never
    /// touches stdout and is never delivered to the agent — it's a pure
    /// *read spec* the backend reads from `BUCKET_JOBS` at query time and
    /// turns into `json_extract` aggregation SQL. Each entry is one widget
    /// (a `dashboard:` tab groups them); `scope:` selects per-PC vs
    /// fleet-wide rollup. Because it consumes nothing at run time it
    /// composes with every other hint (typically paired with `emit:`,
    /// which produces the events it reads). See [`AggregateWidget`].
    ///
    /// New field ⇒ #492 wire rule (`default` + `skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Vec<AggregateWidget>>,
    /// v0.26: Layer 2 staleness policy (SPEC.md §2.6.2). Controls
    /// what the agent does at fire time when it can't verify the
    /// `script_current` / `script_status` KV values are fresh —
    /// especially relevant for `runs_on: agent` schedules where
    /// the agent may fire from cache while offline. Defaults to
    /// `Staleness::Cached` (silently use cached values), which
    /// matches every pre-v0.26 Manifest.
    #[serde(default)]
    pub staleness: Staleness,
    /// #291: opt-in marker that this job is offered to **end users**
    /// in the Client App's job tabs over KLP (`jobs.list` →
    /// `jobs.execute`). Parallel to [`inventory`] / [`check`] /
    /// [`emit`]: the block's mere presence is the opt-in, and it
    /// groups the end-user presentation fields (name / category /
    /// icon) that only make sense for a user-facing job. `None`
    /// (the default) ⇒ an operator-only job — inventory, checks,
    /// scheduled maintenance — that never surfaces in the catalog.
    ///
    /// The agent re-reads this at every `jobs.list` / `jobs.execute`
    /// (SPEC §2.1), so removing the block takes a job out of a
    /// running client on its next action.
    ///
    /// [`inventory`]: Manifest::inventory
    /// [`check`]: Manifest::check
    /// [`emit`]: Manifest::emit
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientHint>,
    /// Free-form operator taxonomy for the Jobs catalog. Purely a
    /// SPA-side organisational aid — agents / scheduler / projector
    /// never read it — so it carries no runtime semantics and any
    /// string is allowed (`security`, `weekly`, `windows`, …). Jobs
    /// cross-cut (a `check-bitlocker` is at once a health-check, a
    /// security control, and Windows-specific), which is why this is
    /// a multi-valued list rather than the single closed-enum
    /// [`ClientHint::category`] (whose values are the end-user Client
    /// App's tabs, a different concern). The operator Jobs page groups
    /// rows by id-prefix for free; tags add the orthogonal filter axis
    /// prefixes can't express.
    ///
    /// Empty by default (the overwhelming majority of jobs), and a
    /// new field, so it follows the #492 wire rule: `serde(default)`
    /// plus `skip_serializing_if` keep gradually-upgrading old readers
    /// from tripping over its absence / presence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// GitOps provenance (#678) — see [`RepoOrigin`]. Stamped by
    /// `kanade job create` when the source YAML lives inside a Git work
    /// tree, so the SPA can render the job read-only and point edits
    /// back at the repo instead of letting a ClickOps edit silently
    /// diverge from Git (SPEC design principle #3: 設定駆動 YAML + Git).
    /// `None` for SPA-born jobs and for manifests applied from outside
    /// any Git repo. Purely informational: agents / scheduler /
    /// projector never read it, and it survives `script_file:` inlining
    /// (it's orthogonal to the exactly-one-of script-source rule). New
    /// field ⇒ #492 wire rule (`default` + `skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RepoOrigin>,
    /// Job-generic post-step hook. When set, the agent runs this script
    /// AFTER the main `execute:` script exits cleanly (and, for a
    /// `collect:` job, after the bundle finishes uploading), so the
    /// operator can delete / move / notify based on what the step
    /// produced. Best-effort: a finalize failure is logged but never
    /// fails the run — the upload (if any) already succeeded.
    ///
    /// For `collect:` jobs the agent injects the environment variable
    /// `KANADE_COLLECT_RESULT` — a JSON object
    /// `{ "ok": true, "bundles": [ { "key", "uploaded", "files": [...] } ] }`
    /// — so the hook acts on exactly the files that were bundled and
    /// uploaded (e.g. deletes only the `uploaded` ones). Composes with
    /// every hint. New field ⇒ #492 wire rule (`default` +
    /// `skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalize: Option<FinalizeSpec>,
}

/// GitOps provenance for a repo-managed YAML artifact — a [`Manifest`]
/// (#678) or a [`Schedule`] (#695). Populated by `kanade job create` /
/// `kanade schedule create` from the Git context of the source YAML;
/// the SPA reads it to render Git-managed entries read-only and link
/// the operator back at the repo. Never consulted by the runtime.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct RepoOrigin {
    /// Repo-relative path of the source YAML — the primary edit target
    /// the SPA surfaces (e.g. `configs/jobs/foo.yaml`). Forward slashes
    /// regardless of the authoring OS.
    pub path: String,
    /// `origin` remote URL, when the repo has one. Lets the SPA turn
    /// `path` into a clickable link; `None` for remote-less repos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Repo-relative path of the `script_file:` a job manifest inlined,
    /// when it used one — a secondary pointer shown beneath `path`.
    /// Always `None` for schedules (they carry no script).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_file: Option<String>,
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

/// Sentinel lines that fence a hint's structured JSON payload inside an
/// otherwise human-readable job stdout. Each stdout-reading hint
/// (`inventory:` / `check:` / `collect:`) has its OWN `#KANADE-<KIND>-
/// BEGIN`/`-END` pair, so one job can carry several of them at once
/// (and/or a user-facing message) on its single stdout stream — every
/// consumer extracts only its own block via [`fenced_payload`].
///
/// Originated for inventory (#793): a `client:` job couldn't put both a
/// friendly message and a JSON object on one stdout (the Client App
/// renders stdout verbatim, the projector needs JSON). #821 generalised
/// it so inventory / check / collect can coexist. `emit:` is the
/// exception — its stdout is line-delimited NDJSON consumed whole, so it
/// never fences and never coexists with the others.
///
/// A job carrying a SINGLE hint may still skip the fence —
/// [`fenced_payload`] falls back to the whole stdout — but a job
/// COMBINING hints must fence each block (else every consumer would try
/// to parse the same whole stdout).
pub const INVENTORY_BLOCK_BEGIN: &str = "#KANADE-INVENTORY-BEGIN";
/// Closing marker — see [`INVENTORY_BLOCK_BEGIN`].
pub const INVENTORY_BLOCK_END: &str = "#KANADE-INVENTORY-END";
/// Check-payload opening marker — see [`INVENTORY_BLOCK_BEGIN`].
pub const CHECK_BLOCK_BEGIN: &str = "#KANADE-CHECK-BEGIN";
/// Check-payload closing marker.
pub const CHECK_BLOCK_END: &str = "#KANADE-CHECK-END";
/// Collect-payload opening marker — see [`INVENTORY_BLOCK_BEGIN`].
pub const COLLECT_BLOCK_BEGIN: &str = "#KANADE-COLLECT-BEGIN";
/// Collect-payload closing marker.
pub const COLLECT_BLOCK_END: &str = "#KANADE-COLLECT-END";

/// Extract a hint's fenced block when the `begin` marker is present, else
/// `None`. An unterminated fence (closing marker missing, e.g. truncated
/// output) takes everything after the opener. Trimmed so surrounding
/// message text / whitespace never reaches the JSON parser.
pub fn fenced_payload_if_present<'a>(stdout: &'a str, begin: &str, end: &str) -> Option<&'a str> {
    let b = find_line_marker(stdout, begin)?;
    let after = &stdout[b + begin.len()..];
    let inner = match find_line_marker(after, end) {
        Some(e) => &after[..e],
        None => after,
    };
    Some(inner.trim())
}

/// True if stdout carries ANY `#KANADE-<KIND>-BEGIN` fence at a line
/// start — i.e. the script opted into fenced output. Used to decide
/// whether a missing fence means "single-hint, use the whole stdout" or
/// "multi-hint author error / truncation, this hint just has no block".
pub fn has_any_hint_fence(stdout: &str) -> bool {
    [
        INVENTORY_BLOCK_BEGIN,
        CHECK_BLOCK_BEGIN,
        COLLECT_BLOCK_BEGIN,
    ]
    .iter()
    .any(|m| find_line_marker(stdout, m).is_some())
}

/// Extract one hint's JSON payload from a job's stdout. When the hint's
/// own `#KANADE-<KIND>` fence is present, return that block. When it's
/// absent, fall back to the WHOLE stdout only for an unfenced (single-
/// hint) job; if any OTHER hint's fence is present (#821 multi-hint
/// output) return `""` instead — the script opted into fences but this
/// block is missing (author error or truncation), so this consumer must
/// NOT grab a sibling hint's block. An empty payload fails the consumer's
/// JSON parse and degrades to "no data for this hint", never cross-parse.
pub fn fenced_payload<'a>(stdout: &'a str, begin: &str, end: &str) -> &'a str {
    if let Some(p) = fenced_payload_if_present(stdout, begin, end) {
        return p;
    }
    if has_any_hint_fence(stdout) {
        ""
    } else {
        stdout.trim()
    }
}

/// Inventory's fenced payload — [`fenced_payload`] with the inventory
/// markers. Kept as a named helper for the projector call site.
pub fn inventory_payload(stdout: &str) -> &str {
    fenced_payload(stdout, INVENTORY_BLOCK_BEGIN, INVENTORY_BLOCK_END)
}

/// Find `needle` only where it begins a line (start of `hay` or right
/// after a `\n`). Anchoring to line start means a script echoing the
/// literal sentinel mid-message (e.g. printing a command name) can't
/// false-trigger the fence (Claude #793).
fn find_line_marker(hay: &str, needle: &str) -> Option<usize> {
    if hay.starts_with(needle) {
        return Some(0);
    }
    hay.find(&format!("\n{needle}")).map(|p| p + 1)
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

/// Manifest sub-section (#290): marks a job as an operator-defined
/// **health check**. Parallel to [`InventoryHint`] / `EmitConfig`.
/// The stdout contract is a free-form JSON object (same as any
/// inventory job) from which the agent reads `status_field` /
/// `detail_field` to build the KLP [`crate::ipc::state::Check`] shown
/// on the Client App's Health tab.
///
/// There is deliberately **no timing field** — when / how often /
/// in which window a check runs is driven by the job's Schedule,
/// exactly like inventory jobs, so operators get the full `when:` /
/// rollout / `runs_on` expressiveness for free.
///
/// A check's stdout is a **free-form inventory object** (arbitrary
/// key/value pairs + arrays) — same as any inventory job — that also
/// carries a status field. `check:` adds only the health semantics on
/// top: which field is the ok/warn/fail/unknown status, an optional
/// one-line summary field, and a remediation job. Everything else
/// (rich per-PC detail, `explode` sub-tables like a software list) is
/// driven by a co-present [`InventoryHint`] and rendered with the
/// SAME display logic the SPA Inventory page uses — on the Client App
/// too. This keeps checks maximally expressive without a bespoke
/// payload type.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct CheckHint {
    /// Stable check id → [`Check.name`](crate::ipc::state::Check),
    /// the SPA/Client React key + analytics label. Unique within the
    /// fleet's check set. Machine-friendly slug (`disk_space`,
    /// `defender_rtp`); for the human-facing row title see [`label`].
    ///
    /// [`label`]: CheckHint::label
    pub name: String,
    /// Optional human-facing display title →
    /// [`Check.label`](crate::ipc::state::Check). The Client App's
    /// Health tab and the operator SPA's Compliance page render this
    /// instead of the [`name`](CheckHint::name) slug when set
    /// (`"ウイルス対策のリアルタイム保護"` reads better than
    /// `defender_rtp`). Falls back to the slug when absent, so it's
    /// purely additive. Author it in the check's language — there's no
    /// per-locale variant; checks are operator-defined per fleet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Top-level stdout field whose string value
    /// (`ok`/`warn`/`fail`/`unknown`) becomes the Health-tab light
    /// ([`CheckStatus`](crate::ipc::state::CheckStatus)). Defaults to
    /// `"status"`; a missing / unparseable value → `unknown`.
    #[serde(default = "default_status_field")]
    pub status_field: String,
    /// Top-level stdout field used as the Health-tab row's one-line
    /// summary. Defaults to `"detail"`; absent in the payload → no
    /// detail line (the rich breakdown lives in the inventory view).
    #[serde(default = "default_detail_field")]
    pub detail_field: String,
    /// Optional remediation job id →
    /// [`Check.troubleshoot`](crate::ipc::state::Check). The Client
    /// App shows a "修復する" button when present; that job must be
    /// `user_invokable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub troubleshoot: Option<String>,
    /// #290 PR-E: when `true` (default), the backend also projects this
    /// check's `status` / `detail` into the `check_status` table so the
    /// operator SPA gets a fleet-wide compliance view for free — no
    /// `inventory:` block needed. Set `fleet: false` for a client-only
    /// check the operator doesn't want surfaced across the fleet.
    #[serde(default = "default_true")]
    pub fleet: bool,
    /// When `true` (default), this check is shown on the Client App's
    /// Health tab (the end user sees its ok/warn/fail row). Set
    /// `health: false` for a **gate-only** check — one that exists purely
    /// to drive a `client.show_when` display gate (e.g. `myapp-up-to-date`)
    /// and would just be noise as a Health row. The agent still records it
    /// into `StateSnapshot.checks` (so `show_when` can read it and the gate
    /// keeps working); only the Client App's Health *rendering* skips it,
    /// via the [`Check.health_hidden`](crate::ipc::state::Check::health_hidden)
    /// wire flag. Orthogonal to [`fleet`](CheckHint::fleet): `fleet` gates
    /// the operator SPA fleet view, `health` gates the end-user Health tab,
    /// so a pure gate detector typically sets neither (`fleet: false` +
    /// `health: false`) to stay invisible everywhere while still driving
    /// the gate.
    #[serde(default = "default_true")]
    pub health: bool,
    /// Optional auto-notification on a compliance transition. When set, the
    /// backend publishes an end-user notification the moment this check
    /// transitions *into* one of [`CheckAlert::on`] (e.g. ok → fail) — to
    /// the failing PC's user and/or operator groups. Fired once per
    /// transition (not on every poll). Requires `fleet: true` (the alert
    /// rides the same projection that fills `check_status`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<CheckAlert>,
}

/// Auto-notification rule for a [`CheckHint`] (compliance alerting). When a
/// check's status transitions into one of [`on`](Self::on), the backend
/// publishes a notification to the failing PC's user
/// ([`notify_user`](Self::notify_user)) and/or operator groups
/// ([`notify_groups`](Self::notify_groups)). Deliberately config-driven:
/// who gets told, how loud, and the wording all live in the manifest, not
/// hardcoded in the backend.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct CheckAlert {
    /// Statuses that fire the alert on *transition into* them (a check that
    /// stays failing doesn't re-alert every poll). Defaults to `[fail]`.
    /// `ok` is not representable — [`CheckAlertStatus`] has no `Ok` variant,
    /// so a YAML `on: [ok]` fails to deserialize (before `validate()` is
    /// even reached); "recovered" notifications are out of scope.
    #[serde(default = "default_alert_on")]
    pub on: Vec<CheckAlertStatus>,
    /// Notify the user(s) on the failing PC (`notifications.pc.<pc_id>`).
    #[serde(default)]
    pub notify_user: bool,
    /// Notify these operator groups (`notifications.group.<name>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify_groups: Vec<String>,
    /// Notification priority (colour/label only — toasting is the separate
    /// `toast` flag). Defaults to `warn`.
    #[serde(default = "default_alert_priority")]
    pub priority: crate::ipc::notifications::NotificationPriority,
    /// Require the recipient to click 確認 to dismiss.
    #[serde(default)]
    pub require_ack: bool,
    /// Surface an OS toast (launches a closed Client App, Action Center
    /// while locked). Recommended `true` for `notify_user` so a
    /// non-emergency "your PC is non-compliant" nudge still reaches a user
    /// whose app is closed.
    #[serde(default)]
    pub toast: bool,
    /// Also send the alert by email, to every address mapped to the
    /// `notify_groups` (via the `group_contacts` KV, edited on the SPA
    /// Groups page). Opt-in: defaults to `false`, so an existing alert
    /// never starts emailing on its own. Requires `notify_groups` to be
    /// non-empty (there is no per-PC user email) and the backend's
    /// `[mail]` config to be present; otherwise the email is a logged
    /// no-op while the in-app/toast notification still fires.
    #[serde(default)]
    pub email: bool,
    /// Notification title (required). May use the same `{…}` placeholders
    /// as [`body`](Self::body).
    pub title: String,
    /// Notification body template. Placeholders: `{pc_id}`, `{name}` (check
    /// slug), `{label}` (check label, falls back to slug), `{status}`,
    /// `{detail}` (the check's one-line summary), `{last_logon}` (the PC's
    /// last sign-in account). Absent → empty body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// A check status that can trigger a [`CheckAlert`]. Mirrors the
/// projected `check_status.status` values minus `ok` (alerting on `ok` is
/// rejected at validation).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CheckAlertStatus {
    Warn,
    Fail,
    Unknown,
}

impl CheckAlertStatus {
    /// The wire string, matching the projected `check_status.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }
}

fn default_alert_on() -> Vec<CheckAlertStatus> {
    vec![CheckAlertStatus::Fail]
}

fn default_alert_priority() -> crate::ipc::notifications::NotificationPriority {
    crate::ipc::notifications::NotificationPriority::Warn
}

fn default_status_field() -> String {
    "status".to_string()
}

fn default_detail_field() -> String {
    "detail".to_string()
}

fn default_files_field() -> String {
    "files".to_string()
}

/// Fallback cap on a collect bundle's total input size when the
/// manifest's `collect.max_size` is unset. 50 MB (decimal).
pub const DEFAULT_COLLECT_MAX_SIZE: u64 = 50 * 1_000_000;

/// Manifest sub-section (#219): marks a job as a **file collector** and
/// carries how the collected bundle presents in the SPA. Parallel to
/// [`InventoryHint`] / [`CheckHint`] — the block's presence is the
/// opt-in. The script prints a single JSON object on stdout whose
/// [`files_field`](CollectHint::files_field) key holds an array of file
/// paths to bundle (env vars are expanded); the agent zips them and
/// uploads to `OBJECT_COLLECTIONS`. See [`Manifest::collect`].
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct CollectHint {
    /// Operator/end-user-facing title for the collection, shown as the
    /// bundle's heading on the SPA Collect page (and the Client App row
    /// when paired with `client:`). Required; validated non-empty.
    pub name: String,
    /// Optional one-line description of what the bundle contains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Human-readable cap on the bundle's total input size
    /// (`"50MB"`, `"500KB"`, `"1GiB"`). The agent refuses to build a
    /// bundle whose listed files exceed this. `None` ⇒
    /// [`DEFAULT_COLLECT_MAX_SIZE`]. Parsed by [`parse_size_bytes`];
    /// [`Manifest::validate`] rejects an unparseable value at create
    /// time.
    ///
    /// Note: this bounds the **uncompressed** bytes the agent reads off
    /// disk, not the resulting zip. Text logs compress well, so the
    /// download is usually much smaller; many tiny files add a little
    /// per-entry zip overhead. Read it as "how much the agent reads +
    /// packs", not "the exact download size".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<String>,
    /// Top-level stdout JSON key holding the array of file paths to
    /// bundle. Defaults to `"files"`.
    #[serde(default = "default_files_field")]
    pub files_field: String,
}

impl CollectHint {
    /// The effective size cap in bytes — the parsed `max_size` or
    /// [`DEFAULT_COLLECT_MAX_SIZE`] when unset. Assumes `max_size` (if
    /// present) already passed [`Manifest::validate`]; falls back to the
    /// default on a parse error rather than panicking on the fire path.
    pub fn max_size_bytes(&self) -> u64 {
        match &self.max_size {
            Some(s) => parse_size_bytes(s).unwrap_or(DEFAULT_COLLECT_MAX_SIZE),
            None => DEFAULT_COLLECT_MAX_SIZE,
        }
    }
}

/// Parse a human-readable byte size (`"50MB"`, `"500 KB"`, `"1GiB"`,
/// `"1024"`). Decimal units (KB/MB/GB) are 1000-based; binary units
/// (KiB/MiB/GiB) are 1024-based; a bare number (or `B`) is bytes.
/// Case-insensitive. Shared by `collect.max_size` validation and the
/// agent's bundle-size enforcement.
pub fn parse_size_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("size must not be empty".to_string());
    }
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num_str, unit_raw) = t.split_at(split);
    if num_str.is_empty() {
        return Err(format!("size '{s}': missing leading number"));
    }
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("size '{s}': bad number '{num_str}'"))?;
    let mult: u64 = match unit_raw.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        other => {
            return Err(format!(
                "size '{s}': unknown unit '{other}' (use B/KB/MB/GB/KiB/MiB/GiB)"
            ));
        }
    };
    num.checked_mul(mult)
        .ok_or_else(|| format!("size '{s}': overflow"))
}

/// Manifest sub-section (#291): marks a job as **user-invokable**
/// from the Client App and carries how it presents to the end user.
/// Parallel to [`InventoryHint`] / [`CheckHint`] / `EmitConfig` —
/// the block's presence is the opt-in (no separate boolean), and its
/// required fields (`name`, `category`) are enforced by serde at
/// parse time, so a half-filled catalog entry fails
/// `kanade job create` instead of rendering a nameless / tab-less row.
///
/// The agent maps this 1:1 into the KLP
/// [`UserInvokableJob`](crate::ipc::jobs::UserInvokableJob) wire shape
/// that `jobs.list` returns; the Client App renders one row per job in
/// the tab named by `category`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct ClientHint {
    /// End-user-facing title for the job row. The operator-internal
    /// `Manifest::id` slug is rarely what an end user should read, so
    /// this is required (and validated non-empty by
    /// [`Manifest::validate`]). Maps to `UserInvokableJob::display_name`.
    pub name: String,
    /// Optional one-line subtitle under `name` in the Client App.
    /// Distinct from the operator-facing top-level
    /// [`Manifest::description`] — this one is written for the end
    /// user. Maps to `UserInvokableJob::display_description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which Client App tab the job lives in — a **free-form category
    /// key** (#792). The Client App renders one tab per distinct key.
    /// Well-known keys (`software_update`, `troubleshoot`, `catalog`)
    /// carry built-in tab labels/icons; any other key defines a new tab
    /// (style it with `category_label` / `category_icon`). Required and
    /// validated non-empty — without it the agent can't place the job.
    /// Note: the `software_update` key also drives the agent's
    /// maintenance / auto-reboot grouping.
    pub category: String,
    /// Optional display name for the category's TAB. Set it on (at least
    /// one of) a custom category's jobs to name the tab; `None` ⇒ a
    /// built-in default for a well-known key, else the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_label: Option<String>,
    /// Optional icon for the category's TAB (lucide name or `data:` URL).
    /// `None` ⇒ Client App default for the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_icon: Option<String>,
    /// Optional sort order for the TAB; lower sorts first. `None` ⇒
    /// default (well-known keys keep their familiar order; custom keys
    /// sort after, then by label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_order: Option<i64>,
    /// Optional icon hint for the job ROW — a lucide-react icon name
    /// or a `data:` URL. `None` ⇒ the Client App falls back to the
    /// category's icon. Surfaced verbatim in `jobs.list[].icon`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Optional visibility scope for the end-user Client App (#816).
    ///
    /// `None` ⇒ visible to every PC (current behavior). When set, only
    /// agents whose `pc_id` / group membership match the [`Target`] list
    /// the job in `jobs.list` and may run it via KLP `jobs.execute`.
    ///
    /// This gates the END-USER surface ONLY. Operators are unaffected:
    /// `POST /api/exec/{job_id}` (SPA / `kanade exec`) is a separate path
    /// that never consults `client:`, so an operator can still run the
    /// job on any PC regardless of `visible_to`. Reuses the schedule
    /// `Target` shape (`all` / `groups` / `pcs`); a present-but-empty
    /// target is rejected by [`Manifest::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_to: Option<Target>,
    /// Optional **dynamic display gate** keyed on a health check's result.
    ///
    /// `None` ⇒ always listed (current behavior). When set, the agent
    /// lists the job in `jobs.list` ONLY while the named [`check:`] slug's
    /// latest result is one of [`ShowWhen::is`]. The canonical use is an
    /// update action that hides itself once the machine is already current:
    /// pair the update job with a `check:` that reports `ok` when up to
    /// date and gate on `is: [fail]`.
    ///
    /// Evaluated agent-side at `jobs.list` time against the live
    /// `StateSnapshot.checks`, which is **keyed by check name** — so the
    /// detector `check:` and this job may live in *different* manifests and
    /// still share one slug. Distinct from [`visible_to`](ClientHint::visible_to):
    /// that gates BOTH listing and `jobs.execute` (an authorization
    /// boundary); `show_when` gates listing ONLY (a UX hint), so it can't
    /// cause a list/execute race. New field ⇒ #492 wire rule.
    ///
    /// [`check:`]: crate::manifest::CheckHint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_when: Option<ShowWhen>,
}

/// Dynamic display gate for a [`ClientHint`] — see
/// [`ClientHint::show_when`]. Shows the job only while the named check's
/// latest status is one of [`is`](ShowWhen::is).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct ShowWhen {
    /// The `check:` slug (a [`CheckHint::name`](crate::manifest::CheckHint::name))
    /// whose latest status gates this job. May be defined by a *different*
    /// manifest: checks are keyed by name in the agent's snapshot, so a
    /// standalone detector job and this one can share a slug. A check that
    /// has never run (absent from the snapshot) does NOT match — the job
    /// stays hidden until the detector first reports (fails closed, like
    /// `visible_to`).
    pub check: String,
    /// The check status(es) in which the job is SHOWN. Accepts a single
    /// status (`is: fail`) or a list (`is: [fail, unknown]`); both
    /// deserialize to a `Vec`. The `length(min = 1)` schema constraint +
    /// [`Manifest::validate`] both reject an empty set (it would match
    /// nothing and silently hide the job) so schema-driven tooling and the
    /// write path agree.
    #[serde(deserialize_with = "de_one_or_many_check_status")]
    #[schemars(length(min = 1))]
    pub is: Vec<crate::ipc::state::CheckStatus>,
}

/// Accept either a single `CheckStatus` (`is: fail`) or a sequence
/// (`is: [fail, unknown]`) for [`ShowWhen::is`], normalising to a `Vec`.
/// The scalar form is purely author ergonomics; the JSON schema advertises
/// the canonical array form (`#[schemars(with = ...)]`).
fn de_one_or_many_check_status<'de, D>(
    d: D,
) -> Result<Vec<crate::ipc::state::CheckStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use crate::ipc::state::CheckStatus;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(CheckStatus),
        Many(Vec<CheckStatus>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(c) => vec![c],
        OneOrMany::Many(v) => v,
    })
}

/// #720 — one widget on the SPA **Analytics** page: a declarative
/// aggregation over the `obs_events` table. The backend reads these off
/// `Manifest::aggregate` (from `BUCKET_JOBS`) at query time and builds
/// the `json_extract` GROUP BY / time-bucket SQL from these generic
/// primitives, so an operator can chart any emitted event without a Rust
/// change. The reference shapes are the attendance dashboards
/// (presence / app_sample / web_visit), but the same DSL covers logon /
/// reboot / agent-health trends, etc.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct AggregateWidget {
    /// Tab this widget lives under on the Analytics page. Widgets from
    /// every job are collected and grouped by this label, so the same
    /// string across jobs builds one multi-source dashboard. Required.
    pub dashboard: String,
    /// Widget heading. Required, validated non-empty.
    pub title: String,
    /// Optional one-line subtitle shown muted under the `title` on the
    /// Analytics page — room for a unit, a caveat, or what the number
    /// means ("samples × 2 min", "Security 4624 only"). Rejected if
    /// present-but-blank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional sort weight (#743). Once the order-aware sort lands (PR2)
    /// widgets render in `(order, dashboard, title)` order, so a lower
    /// `order` pulls a widget — and its tab — earlier; equal/absent `order`
    /// falls back to the alphabetical `(dashboard, title)` ordering. Treated
    /// as `0` when unset, so a fleet with no `order` anywhere stays purely
    /// alphabetical (today's behaviour); negatives are allowed to pin
    /// something first. (This field only carries the value; the backend
    /// applies it.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    /// `pc` rolls up a single selected PC; `fleet` rolls up all PCs
    /// (and unlocks `group_by: pc_id` to rank PCs against each other).
    /// Defaults to `pc`.
    #[serde(default)]
    pub scope: AggregateScope,
    /// `obs_events.kind` this widget reads (e.g. `app_sample`,
    /// `presence`, `unexpected_shutdown`). Required for every aggregation
    /// render (`bar`/`gauge`/`timeline`/`stat`); rejected for
    /// `op_timeline`, which reconstructs a fixed multi-kind operational
    /// swimlane (power/session/sleep) baked into the SPA and so reads no
    /// single `kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Optional `obs_events.source` filter, when one `kind` is emitted by
    /// more than one collector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// How to roll the matching events up. See [`AggregateAgg`]. Required
    /// for every aggregation render; rejected for `op_timeline` (which
    /// performs no rollup — it returns the raw operational events and the
    /// SPA folds them into lane spans).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agg: Option<AggregateAgg>,
    /// Dotted JSON path (no `$.` prefix) to group by for `agg: count` /
    /// `sum` — e.g. `foreground.app`. The literal `pc_id` is special:
    /// it groups by the `pc_id` column (fleet ranking), not a payload
    /// field. Omit for a single total. Required when `agg: sum` needs a
    /// breakdown; for `agg: count` omitting it yields the grand total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    /// Dotted JSON path to a boolean for `agg: ratio` (e.g. `active`):
    /// the widget reports `true_count / total`. Required when `agg: ratio`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bool_path: Option<String>,
    /// Dotted JSON path to a number for `agg: sum`. Required when `agg: sum`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_path: Option<String>,
    /// Optional value transform applied before grouping. Currently only
    /// `host` (parse a URL down to its host) — used by the top-sites
    /// widget, where SQLite can't parse a URL so the backend does it in
    /// Rust. See [`AggregateTransform`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<AggregateTransform>,
    /// Optional sampling cadence in minutes. When set, a `count` is also
    /// reported as estimated time (`count × sample_minutes`) — e.g. a
    /// 2-minute app sampler turns 11 samples into ~22 minutes. Must be ≥ 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub sample_minutes: Option<u32>,
    /// Grouped values to drop from the rollup (e.g. `["LockApp"]` so the
    /// lock screen doesn't top the app ranking). Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Optional time bucketing — `hour` buckets events by local
    /// hour-of-day for a `timeline` render. See [`AggregateTimeBucket`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_bucket: Option<AggregateTimeBucket>,
    /// Top-N cap for grouped renders (`bar`). Defaults to 10 when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub limit: Option<u32>,
    /// Which widget the SPA draws. See [`AggregateRender`].
    pub render: AggregateRender,
}

/// Per-PC vs fleet-wide rollup for an [`AggregateWidget`].
#[derive(
    Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AggregateScope {
    /// Roll up the single PC the operator selected. The default.
    #[default]
    Pc,
    /// Roll up across every PC. Unlocks `group_by: pc_id`.
    Fleet,
    /// #492 forward-compat catch-all — a Manifest is read fleet-wide, so
    /// an older reader must tolerate a future variant rather than failing
    /// to decode the whole job. The backend skips an `Unknown` widget.
    #[serde(other)]
    Unknown,
}

/// The rollup function for an [`AggregateWidget`].
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AggregateAgg {
    /// Row count, optionally grouped (`group_by`) and time-estimated
    /// (`sample_minutes`).
    Count,
    /// `true_count / total` over `bool_path`.
    Ratio,
    /// Sum of `value_path`, optionally grouped.
    Sum,
    /// #492 forward-compat catch-all (see [`AggregateScope::Unknown`]).
    #[serde(other)]
    Unknown,
}

/// Optional pre-grouping value transform for an [`AggregateWidget`].
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AggregateTransform {
    /// Parse the grouped value as a URL and keep only its host.
    Host,
    /// #492 forward-compat catch-all (see [`AggregateScope::Unknown`]).
    #[serde(other)]
    Unknown,
}

/// Time bucketing for an [`AggregateWidget`] (drives a `timeline`).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AggregateTimeBucket {
    /// Bucket by local hour-of-day (0–23), summed over the window.
    Hour,
    /// #492 forward-compat catch-all (see [`AggregateScope::Unknown`]).
    #[serde(other)]
    Unknown,
}

/// Which visual the SPA renders an [`AggregateWidget`] as.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AggregateRender {
    /// Ranked horizontal bars (a grouped `count` / `sum`).
    Bar,
    /// A single ratio dial (`agg: ratio`).
    Gauge,
    /// 24-hour activity strip (`time_bucket: hour`).
    Timeline,
    /// A single headline number (an ungrouped total).
    Stat,
    /// Per-PC operational swimlane (power / session / sleep) reconstructed
    /// from a fixed multi-kind event set. Unlike the aggregation renders it
    /// reads no single `kind`/`agg`: the backend returns the raw events in
    /// the window and the SPA folds them into lane spans (shared with the
    /// Events page strip). Per-PC only (`scope: pc`).
    #[serde(rename = "op_timeline")]
    OpTimeline,
    /// #492 forward-compat catch-all (see [`AggregateScope::Unknown`]).
    #[serde(other)]
    Unknown,
}

/// True if `p` is a well-formed dotted JSON path of `[A-Za-z0-9_]`
/// segments joined by single dots — the shape safe to bind into
/// `json_extract(payload, '$.' || ?)`. The charset blocks injection; the
/// segment check additionally rejects `"."`, `".foo"`, `"foo."`,
/// `"foo..bar"`, which would pass the charset but produce a malformed
/// `$.` path that errors at query time. Accepts `pc_id`, `foreground.app`,
/// `active`, etc.
fn is_valid_json_path(p: &str) -> bool {
    !p.is_empty()
        && p.split('.').all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// Per-widget validation for a list of [`AggregateWidget`]s — shared by
/// the `aggregate:` job hint ([`Manifest::validate`]) and the standalone
/// [`View`] resource (#743) so the two can't diverge. `field` names the
/// containing key for error messages (`"aggregate"` or `"widgets"`).
///
/// Enforces: non-empty list; non-empty dashboard/title (and `kind`/`agg`
/// for every aggregation render); a blank-when-set `source`; rejection of
/// any #492 `Unknown` enum (an operator typo at create time); safe dotted
/// JSON paths; the value path each `agg` needs (and rejection of mis-paired
/// ones); `pc_id` grouping only in `fleet` scope; `transform`/`limit`/
/// `exclude` only with a `group_by`; positive `limit`/`sample_minutes`;
/// `gauge`⇔`ratio`; and `timeline`⇔`time_bucket`. A `render: op_timeline`
/// widget is validated separately (per-PC, no aggregation knobs) — see
/// [`validate_op_timeline_widget`].
pub fn validate_aggregate_widgets(widgets: &[AggregateWidget], field: &str) -> Result<(), String> {
    if widgets.is_empty() {
        return Err(format!(
            "`{field}:` must list at least one widget when present"
        ));
    }
    for (i, w) in widgets.iter().enumerate() {
        let at = format!("{field}[{i}]");
        for (label, value) in [("dashboard", &w.dashboard), ("title", &w.title)] {
            if value.trim().is_empty() {
                return Err(format!("{at}.{label} must not be empty"));
            }
        }
        // A present-but-blank `description` renders an empty muted line —
        // reject it so the subtitle only shows when it says something.
        if let Some(description) = &w.description {
            if description.trim().is_empty() {
                return Err(format!("{at}.description must not be empty when set"));
            }
        }
        // Reject values that fell through to the #492 `Unknown` catch-all:
        // at create time on the current version that's an operator typo. (A
        // genuinely-future variant only reaches an older reader via a stored
        // resource, which is never re-validated, so forward-compat holds.)
        if w.scope == AggregateScope::Unknown {
            return Err(format!("{at}.scope is not a known value (pc | fleet)"));
        }
        if w.render == AggregateRender::Unknown {
            return Err(format!(
                "{at}.render is not a known value (bar | gauge | timeline | stat | op_timeline)"
            ));
        }
        // `op_timeline` reconstructs a fixed per-PC operational swimlane
        // (power/session/sleep) from a baked-in multi-kind set — it uses none
        // of the aggregation knobs, so validate it on its own terms (per-PC,
        // no `kind`/`agg`/grouping) and skip the rollup rules below.
        if w.render == AggregateRender::OpTimeline {
            validate_op_timeline_widget(w, &at)?;
            continue;
        }
        // Every other render is an aggregation over a single `kind`.
        if w.kind.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err(format!("{at}.kind must not be empty"));
        }
        let agg = match w.agg {
            Some(AggregateAgg::Unknown) => {
                return Err(format!(
                    "{at}.agg is not a known value (count | ratio | sum)"
                ));
            }
            Some(agg) => agg,
            None => return Err(format!("{at}.agg is required")),
        };
        // A present-but-blank `source` is a no-op filter — reject like the
        // other blank-when-set guards.
        if let Some(source) = &w.source {
            if source.trim().is_empty() {
                return Err(format!("{at}.source must not be empty when set"));
            }
        }
        if w.transform == Some(AggregateTransform::Unknown) {
            return Err(format!("{at}.transform is not a known value (host)"));
        }
        if w.time_bucket == Some(AggregateTimeBucket::Unknown) {
            return Err(format!("{at}.time_bucket is not a known value (hour)"));
        }
        for (label, path) in [
            ("group_by", &w.group_by),
            ("bool_path", &w.bool_path),
            ("value_path", &w.value_path),
        ] {
            if let Some(p) = path {
                if !is_valid_json_path(p) {
                    return Err(format!(
                        "{at}.{label} '{p}' must be a dotted JSON path of [A-Za-z0-9_] segments"
                    ));
                }
            }
        }
        // Each agg uses exactly one value path; reject a mis-paired path so
        // a typo fails at create rather than being ignored.
        match agg {
            // count: grouped → ranking, ungrouped → grand total.
            AggregateAgg::Count => {
                for (label, path) in [("bool_path", &w.bool_path), ("value_path", &w.value_path)] {
                    if path.is_some() {
                        return Err(format!("{at}.agg=count does not use `{label}`"));
                    }
                }
            }
            AggregateAgg::Ratio => {
                if w.bool_path.is_none() {
                    return Err(format!("{at}.agg=ratio requires `bool_path`"));
                }
                if w.value_path.is_some() {
                    return Err(format!("{at}.agg=ratio does not use `value_path`"));
                }
            }
            AggregateAgg::Sum => {
                if w.value_path.is_none() {
                    return Err(format!("{at}.agg=sum requires `value_path`"));
                }
                if w.bool_path.is_some() {
                    return Err(format!("{at}.agg=sum does not use `bool_path`"));
                }
            }
            // Rejected above; arm exists only for exhaustiveness.
            AggregateAgg::Unknown => {}
        }
        // Ranking PCs against each other only means something across the
        // fleet — within one PC it's a single bar.
        if w.group_by.as_deref() == Some("pc_id") && w.scope != AggregateScope::Fleet {
            return Err(format!(
                "{at}.group_by: pc_id is only valid with scope: fleet"
            ));
        }
        // `transform` rewrites the grouped PAYLOAD value (URL→host); it's
        // meaningless on a `pc_id` grouping (the pc_id column, not a payload
        // field), so reject the combo at create time.
        if w.transform.is_some() && w.group_by.as_deref() == Some("pc_id") {
            return Err(format!("{at}.transform is not valid with group_by: pc_id"));
        }
        // limit / transform / exclude all operate on grouped values, so
        // without a `group_by` they're silent no-ops — reject.
        if w.group_by.is_none() {
            if w.limit.is_some() {
                return Err(format!("{at}.limit requires `group_by`"));
            }
            if w.transform.is_some() {
                return Err(format!("{at}.transform requires `group_by`"));
            }
            if !w.exclude.is_empty() {
                return Err(format!("{at}.exclude requires `group_by`"));
            }
        }
        if w.limit == Some(0) {
            return Err(format!("{at}.limit must be > 0"));
        }
        if w.sample_minutes == Some(0) {
            return Err(format!("{at}.sample_minutes must be > 0"));
        }
        for ex in &w.exclude {
            if ex.trim().is_empty() {
                return Err(format!("{at}.exclude must not contain empty entries"));
            }
        }
        // A gauge draws a single ratio dial — only meaningful for agg: ratio.
        if w.render == AggregateRender::Gauge && agg != AggregateAgg::Ratio {
            return Err(format!("{at}.render=gauge is only valid with agg: ratio"));
        }
        // A timeline needs a bucket; a bucket on any other render is a no-op
        // that signals operator confusion — reject both.
        match (w.render, &w.time_bucket) {
            (AggregateRender::Timeline, None) => {
                return Err(format!("{at}.render=timeline requires `time_bucket`"));
            }
            (r, Some(_)) if r != AggregateRender::Timeline => {
                return Err(format!(
                    "{at}.time_bucket is only valid with render: timeline"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate a `render: op_timeline` widget. It draws a fixed per-PC
/// operational swimlane (power / session / sleep) reconstructed by the SPA
/// from a baked-in multi-kind event set, so it uses none of the aggregation
/// knobs: require `scope: pc` and reject every field that only makes sense
/// for a rollup (`kind`/`source`/`agg`/`group_by`/`bool_path`/`value_path`/
/// `transform`/`sample_minutes`/`exclude`/`time_bucket`/`limit`). Rejecting
/// the unused fields (rather than ignoring them) keeps an operator typo from
/// silently doing nothing, matching the rest of this validator.
fn validate_op_timeline_widget(w: &AggregateWidget, at: &str) -> Result<(), String> {
    // Per-PC only: a fleet-wide swimlane of every PC's spans is unbounded
    // and unreadable, and the backend only computes it in per-PC scope.
    if w.scope != AggregateScope::Pc {
        return Err(format!("{at}.render=op_timeline requires scope: pc"));
    }
    // Each unused field, with the name the operator wrote, so the error
    // points at exactly what to delete.
    if w.kind.is_some() {
        return Err(format!("{at}.render=op_timeline does not use `kind`"));
    }
    if w.source.is_some() {
        return Err(format!("{at}.render=op_timeline does not use `source`"));
    }
    if w.agg.is_some() {
        return Err(format!("{at}.render=op_timeline does not use `agg`"));
    }
    for (label, set) in [
        ("group_by", w.group_by.is_some()),
        ("bool_path", w.bool_path.is_some()),
        ("value_path", w.value_path.is_some()),
        ("transform", w.transform.is_some()),
        ("sample_minutes", w.sample_minutes.is_some()),
        ("time_bucket", w.time_bucket.is_some()),
        ("limit", w.limit.is_some()),
        ("exclude", !w.exclude.is_empty()),
    ] {
        if set {
            return Err(format!("{at}.render=op_timeline does not use `{label}`"));
        }
    }
    Ok(())
}

/// A standalone declarative read/aggregation for the Analytics page (#743).
///
/// A **view** aggregates stored fleet data (`obs_events`, …) without an
/// `execute` or a schedule — unlike a [`Manifest`] it only declares
/// [`AggregateWidget`]s. (The first line is concise on purpose: `schemars`
/// uses it as the generated schema's `title`.) The backend reads views from
/// `BUCKET_VIEWS` at
/// query time and merges their widgets with the co-located `aggregate:`
/// hints on jobs, so a cross-cutting dashboard (one that charts events
/// emitted by several other jobs / the agent) has a home that doesn't need
/// a noop job carrier. Stored JSON in `BUCKET_VIEWS`, keyed by `id`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct View {
    /// Stable identifier (the KV key). Required, validated non-empty.
    pub id: String,
    /// Optional human description shown on the Views admin page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The widgets this view contributes to the Analytics page.
    pub widgets: Vec<AggregateWidget>,
    /// Free-form operator taxonomy (same role as [`Manifest::tags`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// GitOps provenance (#678), stamped by `kanade view create` from the
    /// source YAML's Git context — same as [`Manifest::origin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RepoOrigin>,
}

/// True if `id` is a safe resource identifier — non-empty and only
/// `[A-Za-z0-9._-]`. A view `id` becomes a NATS KV key *and* a URL path
/// segment (`/api/views/{id}`), so this blocks `/`, `..`, whitespace and
/// other characters that would break the KV key or let a CLI arg wander
/// the URL space. (#743 / #744 follow-up — a deliberately small charset
/// rather than the looser set NATS technically allows.)
pub fn is_valid_resource_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

impl View {
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_resource_id(self.id.trim()) {
            return Err(
                "view.id must be non-empty and only [A-Za-z0-9._-] (it's a KV key + URL segment)"
                    .to_string(),
            );
        }
        validate_aggregate_widgets(&self.widgets, "widgets")?;
        for tag in &self.tags {
            if tag.trim().is_empty() {
                return Err("tags must not contain empty entries".to_string());
            }
        }
        Ok(())
    }
}

/// Issue #246 — `emit:` manifest block for jobs whose stdout is
/// NDJSON observability events (one `ObsEvent` per line). Parallel
/// to `inventory:` but for the append-only timeline pipeline; see
/// `Manifest::emit` for the full contract.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
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

    /// Whether a PC (its `pc_id` + group membership) falls in this target:
    /// `all`, or the pc is listed, or it belongs to a listed group. Used
    /// by the agent to scope `client.visible_to` (#816). An unspecified
    /// target matches nobody (callers should treat "no target" as
    /// "visible to all" before calling this).
    pub fn matches(&self, pc_id: &str, groups: &[String]) -> bool {
        self.all
            || self.pcs.iter().any(|p| p == pc_id)
            || self.groups.iter().any(|g| groups.contains(g))
    }
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
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
    /// Fully wired (#210/#211): the backend resolves the digest at
    /// exec submission (`api::exec::resolve_script_source`), the agent
    /// fetches + sha-verifies + caches the body (`script_cache`), and
    /// `kanade script` CRUDs the store. Unlike `script_file:` (inlined
    /// CLI-side, git-managed), this keeps the body in versioned,
    /// digest-pinned object storage — the ops-managed counterpart.
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

/// Job-generic post-step hook (see [`Manifest::finalize`]). Runs after
/// the main `execute:` script (and the collect upload) on a clean exit,
/// with the step's structured result injected via an environment
/// variable. P1 supports an inline `script:` only — `script_file:` /
/// `script_object:` are follow-ups.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct FinalizeSpec {
    pub shell: ExecuteShell,
    /// Inline script body (required; inline-only in P1).
    pub script: String,
    /// humantime duration string (e.g. `"60s"`, `"5m"`). Defaults to
    /// `60s` when unset.
    #[serde(default = "default_finalize_timeout")]
    pub timeout: String,
    /// Token + session combination, like [`Execute::run_as`]. Defaults
    /// to [`RunAs::System`].
    #[serde(default)]
    pub run_as: RunAs,
    /// Working directory for the hook child, like [`Execute::cwd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Default `finalize.timeout` when the operator omits it.
fn default_finalize_timeout() -> String {
    "60s".to_string()
}

impl FinalizeSpec {
    /// Lower to the wire form forwarded onto a [`Command`]. The timeout
    /// parse falls back to 60s — [`Manifest::validate`] already rejects
    /// an unparseable value at create time, so the fire path uses a safe
    /// default rather than failing (mirrors
    /// [`CollectHint::max_size_bytes`]). A sub-second timeout floors at
    /// 1s for the same reason `build_command` does.
    pub fn lower(&self) -> FinalizeCommand {
        let timeout_secs = humantime::parse_duration(&self.timeout)
            .map(|d| d.as_secs().max(1))
            .unwrap_or(60);
        FinalizeCommand {
            shell: self.shell.into(),
            script: self.script.clone(),
            timeout_secs,
            run_as: self.run_as,
            cwd: self.cwd.clone(),
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
        // A present-but-empty finalize script is an invisible no-op
        // (the hook would run an empty body); reject it at the write
        // boundary. Inline-only in P1, so `script` is the sole source.
        if let Some(finalize) = &self.finalize {
            if finalize.script.trim().is_empty() {
                return Err("finalize.script must not be empty".to_string());
            }
            // Reject an unparseable timeout at the write boundary so the
            // operator sees the error at `job create` rather than getting
            // a silent fire-time fallback (`FinalizeSpec::lower` floors to
            // 60s, which would otherwise mask a typo).
            if humantime::parse_duration(&finalize.timeout).is_err() {
                return Err(format!(
                    "finalize.timeout '{}' is not a valid duration",
                    finalize.timeout
                ));
            }
            // Disallow cmd for finalize: the agent injects the result JSON
            // into the hook's environment, and cmd.exe quoting doesn't
            // nest — JSON's `"` plus shell metacharacters in a collected
            // path/key could break out into command injection at the
            // agent's (often LocalSystem) privilege. PowerShell's
            // single-quote escaping is safe, and finalize hooks are
            // PowerShell by convention anyway.
            if finalize.shell == ExecuteShell::Cmd {
                return Err(
                    "finalize.shell: cmd is not supported for finalize hooks (shell-injection \
                     risk when the result JSON is injected into the environment); use powershell"
                        .to_string(),
                );
            }
        }
        // Stdout-format compatibility (#821). `inventory:` / `check:` /
        // `collect:` now COMPOSE: each reads its own `#KANADE-<KIND>-
        // BEGIN/END`-fenced JSON block from stdout, so a single job can
        // project inventory facts, drive a Health-tab check, AND collect
        // files in one run. (A single-hint job may still skip the fence;
        // a multi-hint job must fence each block.)
        //
        // `emit:` remains the exception — its stdout is line-delimited
        // NDJSON consumed whole and then omitted from the result — so it
        // can't share stdout with any fenced hint.
        if self.emit.is_some()
            && (self.inventory.is_some() || self.check.is_some() || self.collect.is_some())
        {
            return Err(
                "`emit:` is incompatible with `inventory:` / `check:` / `collect:` — emit's \
                 stdout is NDJSON timeline events (consumed whole and omitted from the result), \
                 while the others read fenced JSON blocks from stdout"
                    .to_string(),
            );
        }
        // A check's `name` is the Health-tab row id (React key); the
        // field names tell the agent where to read status/detail.
        // An empty value is an invisible runtime bug, and the serde
        // defaults don't guard an operator who writes `status_field:
        // ""` explicitly — reject all three here.
        if let Some(check) = &self.check {
            for (label, value) in [
                ("check.name", &check.name),
                ("check.status_field", &check.status_field),
                ("check.detail_field", &check.detail_field),
            ] {
                if value.trim().is_empty() {
                    return Err(format!("{label} must not be empty"));
                }
            }
            // A present-but-blank `troubleshoot` is a broken
            // remediation job id (the "修復する" button would target
            // an empty manifest id) — reject it too.
            if let Some(troubleshoot) = &check.troubleshoot {
                if troubleshoot.trim().is_empty() {
                    return Err("check.troubleshoot must not be empty when set".to_string());
                }
            }
            // A present-but-blank `label` would render an empty row
            // title on the Health tab / Compliance page — reject it so
            // the slug fallback only ever kicks in when label is absent.
            if let Some(label) = &check.label {
                if label.trim().is_empty() {
                    return Err("check.label must not be empty when set".to_string());
                }
            }
            if let Some(alert) = &check.alert {
                // An alert that names no recipient is a silent no-op.
                if !alert.notify_user && alert.notify_groups.is_empty() {
                    return Err("check.alert must set notify_user and/or notify_groups".to_string());
                }
                if alert.title.trim().is_empty() {
                    return Err("check.alert.title must not be empty".to_string());
                }
                // `on: []` would never fire; an empty group name resolves to
                // a malformed `notifications.group.` subject.
                if alert.on.is_empty() {
                    return Err("check.alert.on must list at least one status".to_string());
                }
                if alert.notify_groups.iter().any(|g| g.trim().is_empty()) {
                    return Err("check.alert.notify_groups must not contain blanks".to_string());
                }
                // Email is addressed via group_contacts (group → email), so
                // there must be a group to map. notify_user has no email.
                if alert.email && alert.notify_groups.is_empty() {
                    return Err(
                        "check.alert.email requires notify_groups (email is addressed per group, not per user)"
                            .to_string(),
                    );
                }
                // The alert rides the `check_status` projection, which only
                // runs for `fleet: true`.
                if !check.fleet {
                    return Err(
                        "check.alert requires fleet: true (the alert rides the compliance projection)"
                            .to_string(),
                    );
                }
            }
        }
        // #291: a `client:` job is rendered in the Client App's
        // catalog (`jobs.list` → `jobs.execute`). serde already makes
        // `name` + `category` required at parse time; the only gap is
        // a present-but-blank `name`, which would render an empty row
        // title — reject it like the other display-id fields.
        if let Some(client) = &self.client {
            if client.name.trim().is_empty() {
                return Err("client.name must not be empty".to_string());
            }
            // #792: category is a free-form key now, so a blank one would
            // group the job under an empty tab — reject it like `name`.
            if client.category.trim().is_empty() {
                return Err("client.category must not be empty".to_string());
            }
            // Optional display fields, when present, must be
            // meaningful: a blank `description` renders an empty
            // subtitle and a blank `icon` is a dangling lucide name.
            // Same present-but-blank guard the `check:` block applies
            // to its optional `troubleshoot` id.
            for (label, value) in [
                ("client.description", &client.description),
                ("client.icon", &client.icon),
                ("client.category_label", &client.category_label),
                ("client.category_icon", &client.category_icon),
            ] {
                if let Some(v) = value {
                    if v.trim().is_empty() {
                        return Err(format!("{label} must not be empty when set"));
                    }
                }
            }
            // #816: a present-but-empty `visible_to` (no all/groups/pcs)
            // would hide the job from everyone in the Client App — almost
            // certainly a mistake. Require at least one selector; omit the
            // whole block to mean "visible to all".
            if let Some(t) = &client.visible_to {
                if !t.is_specified() {
                    return Err(
                        "client.visible_to must set at least one of all / groups / pcs (omit it for all PCs)"
                            .to_string(),
                    );
                }
            }
            // show_when: a dynamic display gate keyed on a check result. A
            // malformed check slug matches nothing and an empty status list
            // matches nothing — both would silently hide the job forever,
            // so reject them at create time rather than at a confused
            // "why isn't my job showing?" later. The slug must be a clean
            // resource id (same charset checks/jobs use): a typo with spaces
            // or punctuation can never match a real check name, so catch it
            // here instead of failing closed at runtime. (Whether the slug
            // names a check that actually EXISTS can't be checked here —
            // checks are keyed by name across manifests — so a valid-but-
            // unknown slug stays a runtime miss = hidden, the documented
            // fail-closed behavior.)
            if let Some(sw) = &client.show_when {
                if !is_valid_resource_id(sw.check.trim()) {
                    return Err(
                        "client.show_when.check must be a non-empty check slug ([A-Za-z0-9._-])"
                            .to_string(),
                    );
                }
                if sw.is.is_empty() {
                    return Err(
                        "client.show_when.is must list at least one check status".to_string()
                    );
                }
            }
        }
        // #219: a `collect:` job's `name` heads the bundle on the SPA
        // Collect page (and the Client App row when paired with
        // `client:`), `files_field` tells the agent where to read the
        // path list, and `max_size` must be a parseable size so a typo
        // is caught at create time rather than silently capping the
        // bundle at the default on the fire path.
        if let Some(collect) = &self.collect {
            if collect.name.trim().is_empty() {
                return Err("collect.name must not be empty".to_string());
            }
            if collect.files_field.trim().is_empty() {
                return Err("collect.files_field must not be empty".to_string());
            }
            if let Some(description) = &collect.description {
                if description.trim().is_empty() {
                    return Err("collect.description must not be empty when set".to_string());
                }
            }
            if let Some(max_size) = &collect.max_size {
                parse_size_bytes(max_size).map_err(|e| format!("collect.max_size: {e}"))?;
            }
        }
        // #720/#743: `aggregate:` is a pure read-spec (it never touches
        // stdout and is never sent to an agent), so it composes with every
        // other hint. The per-widget rules are shared with the standalone
        // `view` resource — see [`validate_aggregate_widgets`].
        if let Some(widgets) = &self.aggregate {
            validate_aggregate_widgets(widgets, "aggregate")?;
        }
        // A blank / whitespace-only tag is an invisible operator typo
        // that would render an empty filter chip on the Jobs page —
        // reject it like the other present-but-blank display fields.
        for tag in &self.tags {
            if tag.trim().is_empty() {
                return Err("tags must not contain empty entries".to_string());
            }
        }
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
    fn inventory_payload_extracts_fenced_block() {
        // Readable message + fenced JSON → only the JSON, trimmed.
        let stdout = "Wi-Fi 設定を適用しました。\n\
            #KANADE-INVENTORY-BEGIN\n\
            {\"applied\": true}\n\
            #KANADE-INVENTORY-END\n";
        assert_eq!(inventory_payload(stdout), "{\"applied\": true}");
    }

    #[test]
    fn inventory_payload_falls_back_to_whole_stdout() {
        // No fence (a plain inventory job) → whole stdout, trimmed.
        assert_eq!(
            inventory_payload("  {\"ram_gb\": 16}\n"),
            "{\"ram_gb\": 16}"
        );
    }

    #[test]
    fn inventory_payload_handles_unterminated_fence() {
        // Closing marker missing (e.g. truncated) → everything after the
        // opener, trimmed.
        let stdout = "msg\n#KANADE-INVENTORY-BEGIN\n{\"a\": 1}";
        assert_eq!(inventory_payload(stdout), "{\"a\": 1}");
    }

    #[test]
    fn inventory_payload_ignores_mid_line_sentinel() {
        // The marker echoed mid-line (not at a line start) must NOT be
        // treated as a fence — fall back to the whole stdout.
        let stdout = "see #KANADE-INVENTORY-BEGIN in the docs\nnot json";
        assert_eq!(inventory_payload(stdout), stdout.trim());
    }

    #[test]
    fn fenced_payload_extracts_each_hint_block_independently() {
        // #821: one stdout carrying a user message + all three fenced
        // blocks — every consumer pulls only its own.
        let stdout = "\
done!
#KANADE-INVENTORY-BEGIN
{\"os\":\"win\"}
#KANADE-INVENTORY-END
#KANADE-CHECK-BEGIN
{\"status\":\"ok\"}
#KANADE-CHECK-END
#KANADE-COLLECT-BEGIN
{\"files\":[\"a\"]}
#KANADE-COLLECT-END
";
        assert_eq!(
            fenced_payload(stdout, INVENTORY_BLOCK_BEGIN, INVENTORY_BLOCK_END),
            "{\"os\":\"win\"}"
        );
        assert_eq!(
            fenced_payload(stdout, CHECK_BLOCK_BEGIN, CHECK_BLOCK_END),
            "{\"status\":\"ok\"}"
        );
        assert_eq!(
            fenced_payload(stdout, COLLECT_BLOCK_BEGIN, COLLECT_BLOCK_END),
            "{\"files\":[\"a\"]}"
        );
    }

    #[test]
    fn fenced_payload_falls_back_to_whole_stdout_without_fence() {
        // A single-hint job needs no fence — the whole (trimmed) stdout is
        // the payload.
        let stdout = "  {\"files\":[\"a\"]}  ";
        assert_eq!(
            fenced_payload(stdout, COLLECT_BLOCK_BEGIN, COLLECT_BLOCK_END),
            "{\"files\":[\"a\"]}"
        );
    }

    #[test]
    fn fenced_payload_returns_empty_when_other_fences_present_but_mine_missing() {
        // Multi-hint output (inventory + check fenced) but the COLLECT
        // fence is missing — collect must NOT fall back to the whole
        // stdout (which holds the inventory/check blocks) and cross-parse
        // a sibling block; it gets "" → its JSON parse fails → no data.
        let stdout = "\
#KANADE-INVENTORY-BEGIN
{\"os\":\"win\"}
#KANADE-INVENTORY-END
#KANADE-CHECK-BEGIN
{\"status\":\"ok\"}
#KANADE-CHECK-END
";
        assert_eq!(
            fenced_payload(stdout, COLLECT_BLOCK_BEGIN, COLLECT_BLOCK_END),
            ""
        );
        // ...while the hints that DID fence still extract correctly.
        assert_eq!(
            fenced_payload(stdout, INVENTORY_BLOCK_BEGIN, INVENTORY_BLOCK_END),
            "{\"os\":\"win\"}"
        );
    }

    /// The example check-job + schedule YAMLs shipped under `configs/`
    /// must stay valid as the schema evolves (#290 PR-C). `include_str!`
    /// pins them at compile time so a breaking edit fails `cargo test`
    /// rather than only `kanade job create` at deploy time.
    #[test]
    fn example_check_job_yamls_parse_and_validate() {
        let jobs = [
            (
                "check-bitlocker",
                include_str!("../../../configs/jobs/check-bitlocker.yaml"),
            ),
            (
                "check-av-signature",
                include_str!("../../../configs/jobs/check-av-signature.yaml"),
            ),
            (
                "check-cert-expiry",
                include_str!("../../../configs/jobs/check-cert-expiry.yaml"),
            ),
            (
                "check-disk-space",
                include_str!("../../../configs/jobs/check-disk-space.yaml"),
            ),
            (
                "check-pending-reboot",
                include_str!("../../../configs/jobs/check-pending-reboot.yaml"),
            ),
            (
                "check-defender-rtp",
                include_str!("../../../configs/jobs/check-defender-rtp.yaml"),
            ),
            (
                "check-firewall",
                include_str!("../../../configs/jobs/check-firewall.yaml"),
            ),
        ];
        for (name, yaml) in jobs {
            let m: Manifest =
                serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{name} parse: {e}"));
            m.validate()
                .unwrap_or_else(|e| panic!("{name} validate: {e}"));
            let check = m
                .check
                .as_ref()
                .unwrap_or_else(|| panic!("{name} must carry a check: hint"));
            assert!(!check.name.trim().is_empty(), "{name} check.name empty");
            // These examples all read admin-only WMI / registry / netsh
            // state, so they run_as system. NOTE: that's a property of
            // these particular checks, NOT of the `check:` contract — a
            // check probing user-session state could run_as user.
            assert_eq!(
                m.execute.run_as,
                RunAs::System,
                "{name} should run_as system"
            );
        }
    }

    /// The example user-invokable job YAMLs (#291) shipped under
    /// `configs/jobs/` must stay valid as the `client:` schema
    /// evolves. `include_str!` pins them at compile time so a breaking
    /// edit fails `cargo test`, not `kanade job create` at deploy.
    #[test]
    fn example_client_job_yamls_parse_and_validate() {
        let jobs = [
            (
                "fix-teams-cache",
                "troubleshoot",
                include_str!("../../../configs/jobs/fix-teams-cache.yaml"),
            ),
            (
                "chrome-update",
                "software_update",
                include_str!("../../../configs/jobs/chrome-update.yaml"),
            ),
            (
                "install-slack",
                "catalog",
                include_str!("../../../configs/jobs/install-slack.yaml"),
            ),
            (
                "fix-defender-rtp",
                "troubleshoot",
                include_str!("../../../configs/jobs/fix-defender-rtp.yaml"),
            ),
            // #792 custom category ("settings") + #809 message/inventory.
            (
                "example-power-plan",
                "settings",
                include_str!("../../../configs/jobs/example-power-plan.yaml"),
            ),
            // #792: diagnostics moved to its own "support" tab.
            (
                "collect-diagnostics",
                "support",
                include_str!("../../../configs/jobs/collect-diagnostics.yaml"),
            ),
        ];
        for (id, category, yaml) in jobs {
            let m: Manifest =
                serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{id} parse: {e}"));
            m.validate()
                .unwrap_or_else(|e| panic!("{id} validate: {e}"));
            assert_eq!(m.id, id, "{id} id mismatch");
            let client = m
                .client
                .as_ref()
                .unwrap_or_else(|| panic!("{id} must carry a client: block"));
            assert!(!client.name.trim().is_empty(), "{id} client.name empty");
            assert_eq!(client.category, category, "{id} category");
        }
    }

    /// #219: the shipped `collect:` example must stay valid as the
    /// schema evolves. `include_str!` pins it at compile time so a
    /// breaking edit (or a YAML typo in the PowerShell block) fails
    /// `cargo test` rather than `kanade job create` at deploy. It carries
    /// both `collect:` and `client:` (end-user-triggerable), which must
    /// compose.
    #[test]
    fn example_collect_job_yaml_parses_and_validates() {
        let yaml = include_str!("../../../configs/jobs/collect-diagnostics.yaml");
        let m: Manifest = serde_yaml::from_str(yaml).expect("collect-diagnostics parse");
        m.validate().expect("collect-diagnostics validate");
        assert_eq!(m.id, "collect-diagnostics");
        let collect = m.collect.as_ref().expect("collect: block present");
        assert!(!collect.name.trim().is_empty());
        assert_eq!(collect.files_field, "files");
        assert_eq!(collect.max_size_bytes(), 50_000_000);
        // collect + client compose — the Client App can trigger it.
        assert!(
            m.client.is_some(),
            "collect-diagnostics also carries client:"
        );
    }

    /// The `emit: { type: events }` collector jobs under
    /// `configs/jobs/` feed the obs_events timeline. `include_str!`
    /// pins them at compile time so a breaking edit (e.g. an `emit:`
    /// paired with `check:`/`inventory:`, a bad watermark field, or a
    /// YAML typo in the PowerShell block) fails `cargo test` rather
    /// than `kanade job create` at deploy. Every one must carry an
    /// `emit.type=events` block and NO check/inventory (validate()
    /// rejects the pairing).
    #[test]
    fn example_event_collector_job_yamls_parse_and_validate() {
        let jobs = [
            // collect-winlog-events was retired in #841 PR2 — the scheduled
            // human-session / power timeline is now read natively by the
            // agent (kanade-agent `winlog` module via EvtQuery), no
            // PowerShell job. collect-winlog-logons-all stays as the
            // on-demand forensic all-token-logons companion.
            (
                "collect-winlog-logons-all",
                include_str!("../../../configs/jobs/collect-winlog-logons-all.yaml"),
            ),
            (
                "collect-wlan-events",
                include_str!("../../../configs/jobs/collect-wlan-events.yaml"),
            ),
        ];
        for (id, yaml) in jobs {
            // Strict parse so an unknown-key typo in these fixtures fails
            // here (not silently at deploy) — the runtime Manifest is
            // unknown-key-tolerant, so the lenient serde_yaml::from_str
            // wouldn't catch fixture drift (CodeRabbit #689).
            let m: Manifest =
                crate::strict::from_yaml_str(yaml).unwrap_or_else(|e| panic!("{id} parse: {e}"));
            m.validate()
                .unwrap_or_else(|e| panic!("{id} validate: {e}"));
            assert_eq!(m.id, id, "{id} id mismatch");
            let emit = m
                .emit
                .as_ref()
                .unwrap_or_else(|| panic!("{id} must carry an emit: block"));
            assert_eq!(emit.kind, EmitKind::Events, "{id} emit.type");
            assert!(
                m.check.is_none() && m.inventory.is_none(),
                "{id}: emit jobs must not pair with check/inventory"
            );
        }
    }

    /// The `inventory:` snapshot jobs under `configs/jobs/` project
    /// facts into `inventory_facts` + exploded tables. `include_str!`
    /// pins them at compile time so a breaking edit (bad explode
    /// schema, a YAML typo in the PowerShell block, an `inventory:`
    /// accidentally paired with `emit:`) fails `cargo test` rather
    /// than the projector at deploy. Each must carry an `inventory:`
    /// block and NO emit (validate() rejects the pairing).
    #[test]
    fn example_inventory_job_yamls_parse_and_validate() {
        let jobs = [
            (
                "inventory-hw",
                include_str!("../../../configs/jobs/inventory-hw.yaml"),
            ),
            (
                "inventory-sw",
                include_str!("../../../configs/jobs/inventory-sw.yaml"),
            ),
            (
                "inventory-driver",
                include_str!("../../../configs/jobs/inventory-driver.yaml"),
            ),
        ];
        for (id, yaml) in jobs {
            let m: Manifest =
                serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{id} parse: {e}"));
            m.validate()
                .unwrap_or_else(|e| panic!("{id} validate: {e}"));
            assert_eq!(m.id, id, "{id} id mismatch");
            assert!(m.inventory.is_some(), "{id} must carry an inventory: block");
            assert!(m.emit.is_none(), "{id}: inventory jobs must not set emit:");
        }
    }

    #[test]
    fn example_check_schedule_yamls_parse_and_validate() {
        let schedules = [
            (
                "check-bitlocker",
                include_str!("../../../configs/schedules/check-bitlocker.yaml"),
            ),
            (
                "check-av-signature",
                include_str!("../../../configs/schedules/check-av-signature.yaml"),
            ),
            (
                "check-cert-expiry",
                include_str!("../../../configs/schedules/check-cert-expiry.yaml"),
            ),
            (
                "check-disk-space",
                include_str!("../../../configs/schedules/check-disk-space.yaml"),
            ),
            (
                "check-pending-reboot",
                include_str!("../../../configs/schedules/check-pending-reboot.yaml"),
            ),
            (
                "check-defender-rtp",
                include_str!("../../../configs/schedules/check-defender-rtp.yaml"),
            ),
            (
                "check-firewall",
                include_str!("../../../configs/schedules/check-firewall.yaml"),
            ),
        ];
        for (name, yaml) in schedules {
            let s: Schedule =
                serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{name} schedule parse: {e}"));
            s.validate()
                .unwrap_or_else(|e| panic!("{name} schedule validate: {e}"));
            assert_eq!(s.job_id, name, "{name} schedule must reference its job");
        }
    }

    /// Inventory schedule wrappers (`per_pc` cadence) must stay valid
    /// alongside the schedule schema. `include_str!` pins them so a
    /// breaking edit fails `cargo test`, not `kanade schedule create`.
    #[test]
    fn example_inventory_schedule_yamls_parse_and_validate() {
        let schedules = [
            (
                "inventory-hw",
                include_str!("../../../configs/schedules/inventory-hw.yaml"),
            ),
            (
                "inventory-sw",
                include_str!("../../../configs/schedules/inventory-sw.yaml"),
            ),
            (
                "inventory-driver",
                include_str!("../../../configs/schedules/inventory-driver.yaml"),
            ),
        ];
        for (name, yaml) in schedules {
            let s: Schedule =
                serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{name} schedule parse: {e}"));
            s.validate()
                .unwrap_or_else(|e| panic!("{name} schedule validate: {e}"));
            assert_eq!(s.job_id, name, "{name} schedule must reference its job");
        }
    }

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

    #[test]
    fn manifest_parses_check_job_and_validates() {
        // An operator-defined health check (#290): a `check:` hint +
        // a PowerShell script that prints {status, detail}.
        let yaml = r#"
id: check-bitlocker
version: 0.1.0
execute:
  shell: powershell
  run_as: system
  timeout: 15s
  script: |
    [pscustomobject]@{ status = 'ok'; detail = 'all volumes protected' } | ConvertTo-Json -Compress
check:
  name: bitlocker
  troubleshoot: fix-bitlocker
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let check = m.check.as_ref().expect("check hint present");
        assert_eq!(check.name, "bitlocker");
        assert_eq!(check.troubleshoot.as_deref(), Some("fix-bitlocker"));
        // Field names default to the conventional "status" / "detail".
        assert_eq!(check.status_field, "status");
        assert_eq!(check.detail_field, "detail");
        assert!(m.inventory.is_none() && m.emit.is_none());
        m.validate().expect("check-only manifest passes validation");
    }

    #[test]
    fn manifest_check_defaults_and_custom_fields() {
        // Minimal: only `name`; status/detail fields default.
        let m: Manifest = serde_yaml::from_str(
            r#"
id: check-disk
version: 0.1.0
execute:
  shell: powershell
  script: "[pscustomobject]@{ status = 'ok' } | ConvertTo-Json -Compress"
  timeout: 10s
check:
  name: disk_free
"#,
        )
        .expect("parse");
        let c = m.check.as_ref().unwrap();
        assert_eq!(c.name, "disk_free");
        assert_eq!(c.status_field, "status");
        assert_eq!(c.detail_field, "detail");
        assert!(c.troubleshoot.is_none());
        m.validate().expect("validates");

        // The operator can point status/detail at any field of their
        // free-form inventory object.
        let m2: Manifest = serde_yaml::from_str(
            r#"
id: check-custom
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
check:
  name: patch_level
  status_field: compliance
  detail_field: summary
"#,
        )
        .expect("parse");
        let c2 = m2.check.as_ref().unwrap();
        assert_eq!(c2.status_field, "compliance");
        assert_eq!(c2.detail_field, "summary");
    }

    #[test]
    fn manifest_allows_check_composed_with_inventory() {
        // `check:` + `inventory:` COMPOSE on the same stdout object:
        // status/detail → Health tab, the rest → SPA projection +
        // explode sub-tables. Must pass validation.
        let yaml = r#"
id: check-bitlocker-detailed
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
check:
  name: bitlocker
inventory:
  display:
    - { field: status, label: Status }
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert!(m.check.is_some() && m.inventory.is_some());
        m.validate().expect("check + inventory compose");
    }

    #[test]
    fn manifest_parses_collect_job_and_validates() {
        // #219: a `collect:` hint + a script that lists files on stdout.
        let yaml = r#"
id: collect-diagnostics
version: 0.1.0
execute:
  shell: powershell
  run_as: system
  timeout: 120s
  script: |
    @{ files = @("$env:KANADE_COLLECT_DIR/system.csv") } | ConvertTo-Json
collect:
  name: "Full diagnostics"
  description: "Event logs + process"
  max_size: 50MB
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let c = m.collect.as_ref().expect("collect hint present");
        assert_eq!(c.name, "Full diagnostics");
        assert_eq!(c.files_field, "files"); // default
        assert_eq!(c.max_size_bytes(), 50_000_000);
        m.validate().expect("collect-only manifest validates");
    }

    #[test]
    fn manifest_finalize_powershell_validates_and_lowers() {
        let yaml = r#"
id: collect-fin
version: 0.1.0
execute:
  shell: powershell
  timeout: 120s
  script: |
    @{ files = @() } | ConvertTo-Json
collect:
  name: "diag"
  max_size: 50MB
finalize:
  shell: powershell
  timeout: 30s
  run_as: system
  script: |
    Write-Output "cleanup"
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        m.validate().expect("powershell finalize validates");
        let lowered = m.finalize.as_ref().expect("finalize present").lower();
        assert_eq!(lowered.timeout_secs, 30);
        assert!(matches!(lowered.shell, Shell::Powershell));
    }

    #[test]
    fn manifest_finalize_rejects_cmd_shell() {
        // cmd finalize is an injection risk (the agent injects JSON into
        // the hook's env; cmd.exe quoting doesn't nest) — validate must
        // reject it.
        let yaml = r#"
id: collect-fin-cmd
version: 0.1.0
execute:
  shell: powershell
  timeout: 120s
  script: |
    @{ files = @() } | ConvertTo-Json
finalize:
  shell: cmd
  script: |
    echo hi
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("cmd finalize rejected");
        assert!(err.contains("finalize.shell"), "got: {err}");
    }

    #[test]
    fn manifest_finalize_rejects_empty_script() {
        let yaml = r#"
id: collect-fin-empty
version: 0.1.0
execute:
  shell: powershell
  timeout: 120s
  script: |
    @{ files = @() } | ConvertTo-Json
finalize:
  shell: powershell
  script: "   "
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("empty finalize script rejected");
        assert!(err.contains("finalize.script"), "got: {err}");
    }

    #[test]
    fn manifest_collect_max_size_defaults_when_unset() {
        let m: Manifest = serde_yaml::from_str(
            r#"
id: collect-min
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
collect:
  name: minimal
"#,
        )
        .expect("parse");
        let c = m.collect.as_ref().unwrap();
        assert!(c.max_size.is_none());
        assert_eq!(c.max_size_bytes(), DEFAULT_COLLECT_MAX_SIZE);
        m.validate().expect("validates");
    }

    #[test]
    fn manifest_allows_collect_with_client() {
        // collect composes with client (client doesn't touch stdout):
        // an end user can trigger a collection from the Client App.
        let yaml = r#"
id: collect-diag-client
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
collect:
  name: diagnostics
client:
  name: "Send diagnostics"
  category: troubleshoot
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert!(m.collect.is_some() && m.client.is_some());
        m.validate().expect("collect + client compose");
    }

    #[test]
    fn manifest_allows_inventory_check_collect_coexistence() {
        // #821: the three fenced hints now COMPOSE — each reads its own
        // `#KANADE-<KIND>` stdout block, so one job can do all three.
        let yaml = r#"
id: multi-hint
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
inventory:
  display:
    - { field: status, label: Status }
check:
  name: health
collect:
  name: diag
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        m.validate()
            .expect("inventory + check + collect coexist after #821");
    }

    #[test]
    fn manifest_rejects_emit_combined_with_fenced_hints() {
        // `emit:` consumes stdout as NDJSON (and blanks it), so it still
        // can't share with any fenced hint — inventory, check, OR collect.
        for extra in [
            "inventory:\n  display:\n    - { field: s, label: S }\n",
            "check:\n  name: health\n",
            "collect:\n  name: diag\n",
        ] {
            let yaml = format!(
                "id: bad-emit-mix\nversion: 0.1.0\nexecute:\n  shell: powershell\n  \
                 script: \"echo x\"\n  timeout: 10s\nemit:\n  type: events\n{extra}"
            );
            let m: Manifest = serde_yaml::from_str(&yaml).expect("parse");
            let err = m
                .validate()
                .expect_err("emit + fenced hint must be rejected");
            assert!(err.contains("emit"), "error mentions emit: {err}");
        }
    }

    #[test]
    fn manifest_rejects_collect_empty_name_and_bad_size() {
        let empty_name: Manifest = serde_yaml::from_str(
            r#"
id: c
version: 0.1.0
execute: { shell: powershell, script: "echo x", timeout: 10s }
collect: { name: "  " }
"#,
        )
        .expect("parse");
        assert!(
            empty_name.validate().is_err(),
            "blank collect.name rejected"
        );

        let bad_size: Manifest = serde_yaml::from_str(
            r#"
id: c
version: 0.1.0
execute: { shell: powershell, script: "echo x", timeout: 10s }
collect: { name: diag, max_size: "50 quux" }
"#,
        )
        .expect("parse");
        let err = bad_size.validate().expect_err("bad max_size rejected");
        assert!(err.contains("max_size"), "error mentions max_size: {err}");
    }

    #[test]
    fn parse_size_bytes_units() {
        assert_eq!(parse_size_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_size_bytes("1B").unwrap(), 1);
        assert_eq!(parse_size_bytes("50MB").unwrap(), 50_000_000);
        assert_eq!(parse_size_bytes("500 KB").unwrap(), 500_000);
        assert_eq!(parse_size_bytes("1GiB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size_bytes("2mib").unwrap(), 2 * 1024 * 1024);
        assert!(parse_size_bytes("").is_err());
        assert!(parse_size_bytes("MB").is_err());
        assert!(parse_size_bytes("12 zonks").is_err());
    }

    #[test]
    fn manifest_rejects_check_combined_with_emit() {
        // `emit:` stdout is NDJSON (and omitted from the result), so
        // it can't pair with `check:` (which needs a single JSON
        // object on stdout).
        let yaml = r#"
id: bad-mix
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
check:
  name: bitlocker
emit:
  type: events
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("emit + check must fail");
        assert!(err.contains("incompatible"), "err: {err}");
    }

    #[test]
    fn manifest_rejects_emit_combined_with_inventory() {
        // The other half of the emit-incompatibility condition.
        let yaml = r#"
id: bad-mix-2
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
emit:
  type: events
inventory:
  display:
    - { field: status, label: Status }
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("emit + inventory must fail");
        assert!(err.contains("incompatible"), "err: {err}");
    }

    #[test]
    fn manifest_rejects_empty_check_field_names() {
        // Empty name / status_field / detail_field are invisible
        // runtime bugs (empty React key, agent reads the wrong field)
        // — reject them even though serde supplies non-empty defaults.
        let base = |inner: &str| {
            format!(
                "id: c\nversion: 0.1.0\nexecute:\n  shell: powershell\n  script: \"echo x\"\n  timeout: 10s\ncheck:\n{inner}"
            )
        };
        for inner in [
            "  name: \"\"\n",
            "  name: ok\n  status_field: \"\"\n",
            "  name: ok\n  detail_field: \"   \"\n",
            // present-but-blank troubleshoot → broken remediation id.
            "  name: ok\n  troubleshoot: \"  \"\n",
        ] {
            let m: Manifest = serde_yaml::from_str(&base(inner)).expect("parse");
            let err = m.validate().expect_err("empty field must fail");
            assert!(err.contains("must not be empty"), "err: {err}");
        }
    }

    #[test]
    fn check_alert_decodes_with_defaults_and_validates() {
        let yaml = r#"
id: c
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 10s
check:
  name: bitlocker
  alert:
    notify_user: true
    title: "BitLocker 未準拠"
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        m.validate().expect("valid alert");
        let alert = m.check.unwrap().alert.unwrap();
        // Defaults: on = [fail], priority = warn, body = None.
        assert_eq!(alert.on, vec![CheckAlertStatus::Fail]);
        assert_eq!(
            alert.priority,
            crate::ipc::notifications::NotificationPriority::Warn
        );
        assert!(alert.body.is_none());
        assert!(alert.notify_user);
    }

    #[test]
    fn check_alert_validation_rejects_bad_configs() {
        let base = |alert: &str| {
            format!(
                "id: c\nversion: 0.1.0\nexecute:\n  shell: powershell\n  script: \"echo x\"\n  timeout: 10s\ncheck:\n  name: bitlocker\n  alert:\n{alert}"
            )
        };
        let cases = [
            // No recipient.
            ("    title: t\n", "notify_user and/or notify_groups"),
            // Empty title.
            (
                "    notify_user: true\n    title: \"  \"\n",
                "title must not be empty",
            ),
            // Empty `on`.
            (
                "    notify_user: true\n    title: t\n    on: []\n",
                "on must list at least one status",
            ),
            // Blank group name.
            (
                "    notify_groups: [\"  \"]\n    title: t\n",
                "notify_groups must not contain blanks",
            ),
            // alert requires fleet: true.
            (
                "    notify_user: true\n    title: t\n  fleet: false\n",
                "requires fleet: true",
            ),
            // email opt-in without a group to address.
            (
                "    notify_user: true\n    email: true\n    title: t\n",
                "email requires notify_groups",
            ),
        ];
        for (alert, want) in cases {
            let m: Manifest = serde_yaml::from_str(&base(alert)).expect("parse");
            let err = m.validate().expect_err("bad alert must fail");
            assert!(err.contains(want), "for {alert:?}: got {err}");
        }
    }

    #[test]
    fn manifest_client_absent_by_default() {
        // A plain operator job (the overwhelming majority) carries no
        // `client:` block, so it never surfaces in the end-user
        // catalog.
        let yaml = r#"
id: echo-test
version: 0.0.1
execute:
  shell: powershell
  script: "echo 'kanade'"
  timeout: 30s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert!(m.client.is_none());
        m.validate().expect("operator-only job validates");
    }

    #[test]
    fn manifest_client_parses_and_validates() {
        // The Client App "困ったとき" remediation job shape: a
        // user-invokable troubleshoot job with the end-user fields the
        // KLP `jobs.list` wire needs, grouped under `client:`.
        let yaml = r#"
id: fix-teams-cache
version: 1.0.0
execute:
  shell: powershell
  script: "echo clearing"
  timeout: 60s
client:
  name: "Teams のキャッシュをクリア"
  description: "Teams が重いときに試してください"
  category: troubleshoot
  icon: brush-cleaning
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let c = m.client.as_ref().expect("client block present");
        assert_eq!(c.name, "Teams のキャッシュをクリア");
        assert_eq!(
            c.description.as_deref(),
            Some("Teams が重いときに試してください")
        );
        assert_eq!(c.category, "troubleshoot");
        assert_eq!(c.icon.as_deref(), Some("brush-cleaning"));
        m.validate().expect("user-invokable job validates");
    }

    #[test]
    fn manifest_client_minimal_only_name_and_category() {
        // description + icon are optional; name + category are the
        // serde-required minimum.
        let yaml = r#"
id: install-slack
version: 1.0.0
execute:
  shell: powershell
  script: "echo install"
  timeout: 600s
client:
  name: Slack
  category: catalog
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let c = m.client.as_ref().expect("client present");
        assert_eq!(c.category, "catalog");
        assert!(c.description.is_none() && c.icon.is_none());
        m.validate().expect("minimal client validates");
    }

    #[test]
    fn manifest_client_rejects_blank_name() {
        // serde guarantees `name`/`category` are present; the one gap
        // is a present-but-blank name → empty catalog row title.
        let yaml = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "   "
  category: catalog
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("blank name must fail");
        assert!(err.contains("client.name"), "err: {err}");
    }

    #[test]
    fn manifest_client_rejects_blank_optional_fields() {
        // description / icon are optional, but a present-but-blank
        // value is a bug (empty subtitle / dangling icon name) — reject
        // it, mirroring the check: block's troubleshoot guard.
        for (field, line) in [
            ("client.description", "  description: \"  \"\n"),
            ("client.icon", "  icon: \"\"\n"),
            // #792: the new category tab-metadata fields get the same
            // present-but-blank guard.
            ("client.category_label", "  category_label: \"  \"\n"),
            ("client.category_icon", "  category_icon: \"\"\n"),
        ] {
            let yaml = format!(
                "id: j\nversion: 1.0.0\nexecute:\n  shell: powershell\n  script: \"echo x\"\n  timeout: 30s\nclient:\n  name: A\n  category: catalog\n{line}"
            );
            let m: Manifest = serde_yaml::from_str(&yaml).expect("parse");
            let err = m.validate().expect_err("blank optional field must fail");
            assert!(err.contains(field), "expected {field} in err: {err}");
        }
    }

    #[test]
    fn manifest_client_rejects_blank_category() {
        // #792: category is a free-form key now; serde keeps it required,
        // but a present-but-blank value would group the job under an empty
        // tab — validate() must reject it.
        let yaml = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "A job"
  category: "   "
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("blank category must fail");
        assert!(err.contains("client.category"), "err: {err}");
    }

    #[test]
    fn target_matches_pc_group_and_all() {
        // #816: pc match, group match, all, and the no-match case.
        let by_pc = Target {
            pcs: vec!["PC1".into()],
            ..Default::default()
        };
        assert!(by_pc.matches("PC1", &[]));
        assert!(!by_pc.matches("PC2", &["g1".into()]));

        let by_group = Target {
            groups: vec!["g1".into()],
            ..Default::default()
        };
        assert!(by_group.matches("PC2", &["g1".into()]));
        assert!(!by_group.matches("PC2", &["g2".into()]));

        let all = Target {
            all: true,
            ..Default::default()
        };
        assert!(all.matches("anyPC", &[]));
    }

    #[test]
    fn manifest_client_rejects_empty_visible_to() {
        // #816: a present-but-empty visible_to (no all/groups/pcs) would
        // hide the job from everyone — validate() must reject it.
        let yaml = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "A job"
  category: troubleshoot
  visible_to: {}
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("empty visible_to must fail");
        assert!(err.contains("client.visible_to"), "err: {err}");
    }

    #[test]
    fn manifest_client_accepts_visible_to_groups() {
        let yaml = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "A job"
  category: settings
  visible_to:
    groups: [wifi-affected]
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        m.validate().expect("visible_to with a group validates");
        let vt = m.client.unwrap().visible_to.unwrap();
        assert_eq!(vt.groups, vec!["wifi-affected".to_string()]);
    }

    #[test]
    fn manifest_client_show_when_accepts_scalar_and_seq() {
        use crate::ipc::state::CheckStatus;
        // `is:` accepts a single status (author ergonomics) ...
        let scalar = r#"
id: office-update
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "Office を最新に更新"
  category: software_update
  show_when:
    check: office-up-to-date
    is: fail
"#;
        let m: Manifest = serde_yaml::from_str(scalar).expect("parse scalar");
        m.validate().expect("scalar show_when validates");
        let sw = m.client.unwrap().show_when.unwrap();
        assert_eq!(sw.check, "office-up-to-date");
        assert_eq!(sw.is, vec![CheckStatus::Fail]);

        // ... and a list (e.g. fail-open on a not-yet-run check).
        let seq = scalar.replace("is: fail", "is: [fail, unknown]");
        let m: Manifest = serde_yaml::from_str(&seq).expect("parse seq");
        m.validate().expect("seq show_when validates");
        assert_eq!(
            m.client.unwrap().show_when.unwrap().is,
            vec![CheckStatus::Fail, CheckStatus::Unknown]
        );
    }

    #[test]
    fn manifest_client_show_when_rejects_empty() {
        // A malformed check slug (here: internal spaces — a typo that could
        // never match a real check name) or an empty status list would
        // silently hide the job forever — validate() must reject both.
        let bad_check = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "A job"
  category: software_update
  show_when:
    check: "office up to date"
    is: fail
"#;
        let m: Manifest = serde_yaml::from_str(bad_check).expect("parse");
        let err = m.validate().expect_err("malformed check slug must fail");
        assert!(err.contains("client.show_when.check"), "err: {err}");

        let empty_is = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "A job"
  category: software_update
  show_when:
    check: office-up-to-date
    is: []
"#;
        let m: Manifest = serde_yaml::from_str(empty_is).expect("parse");
        let err = m.validate().expect_err("empty is[] must fail");
        assert!(err.contains("client.show_when.is"), "err: {err}");
    }

    #[test]
    fn manifest_client_requires_category_at_parse() {
        // A `client:` block missing `category` is a hard parse error
        // (serde required field) — no manual validate() needed.
        let yaml = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "A job"
"#;
        let r: Result<Manifest, _> = serde_yaml::from_str(yaml);
        assert!(
            r.is_err(),
            "missing category must be a parse error, got {r:?}"
        );
    }

    #[test]
    fn manifest_client_rejects_unknown_field() {
        // #492: the strict create boundary catches a fat-fingered
        // `displayname:` (with its path) instead of silently
        // dropping it; the tolerant read path accepts it.
        let yaml = r#"
id: j
version: 1.0.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
client:
  name: "A job"
  category: catalog
  displayname: oops
"#;
        let r = crate::strict::from_yaml_str::<Manifest>(yaml);
        let err = r.expect_err("unknown client field must be rejected at the write boundary");
        // serde_ignored renders the Option layer as `?`:
        // `client.?.displayname`. Assert on the leaf key.
        assert!(err.contains("displayname"), "{err}");
        // The READ path tolerates the same payload (gradual-upgrade
        // contract: an old agent must accept a newer writer's field).
        let m: Manifest = serde_yaml::from_str(yaml).expect("tolerant read");
        assert_eq!(m.client.as_ref().map(|c| c.name.as_str()), Some("A job"));
    }

    #[test]
    fn manifest_tags_default_empty() {
        // The overwhelming majority of jobs carry no tags; the field
        // must default to an empty Vec (not fail to parse) and skip
        // serialisation so old readers never see the key.
        let yaml = r#"
id: echo-test
version: 0.0.1
execute:
  shell: powershell
  script: "echo 'kanade'"
  timeout: 30s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert!(m.tags.is_empty());
        m.validate().expect("tag-less job validates");
        // skip_serializing_if = empty ⇒ the key is absent from JSON.
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(
            !json.contains("tags"),
            "empty tags must not serialise: {json}"
        );
    }

    #[test]
    fn manifest_parses_and_validates_tags() {
        let yaml = r#"
id: check-bitlocker
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
tags: [security, windows, health-check]
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(m.tags, vec!["security", "windows", "health-check"]);
        m.validate().expect("tagged job validates");
        // Round-trips through JSON (the wire format the SPA reads).
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(json.contains("\"tags\""), "non-empty tags must serialise");
    }

    #[test]
    fn manifest_rejects_blank_tag() {
        // A whitespace-only tag renders an empty filter chip — reject
        // it at the write boundary like the other blank display fields.
        let yaml = r#"
id: j
version: 0.1.0
execute:
  shell: powershell
  script: "echo x"
  timeout: 30s
tags: [ok, "   "]
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("blank tag must fail");
        assert!(err.contains("tags must not contain empty"), "err: {err}");
    }

    // #720 — wrap an `aggregate:` YAML block (already indented as a
    // top-level key body) into an otherwise-minimal valid manifest.
    fn manifest_with_aggregate(aggregate_block: &str) -> Manifest {
        let yaml = format!(
            "id: t\nversion: 0.0.1\nexecute:\n  shell: powershell\n  script: echo hi\n  timeout: 30s\n{aggregate_block}"
        );
        serde_yaml::from_str(&yaml).expect("parse aggregate manifest")
    }

    #[test]
    fn aggregate_accepts_full_valid_spec() {
        // count+group_by+exclude+sample_minutes, ratio+bool_path,
        // timeline+time_bucket, fleet ranking via group_by: pc_id, and a
        // bare total stat — alongside emit (composes with every hint).
        let m = manifest_with_aggregate(
            "emit:\n  type: events\naggregate:\n\
             - { dashboard: Utilization, title: Top apps, kind: app_sample, agg: count, group_by: foreground.app, sample_minutes: 2, exclude: [LockApp], render: bar }\n\
             - { dashboard: Utilization, title: Active ratio, kind: presence, agg: ratio, bool_path: active, sample_minutes: 5, render: gauge }\n\
             - { dashboard: Utilization, title: By hour, kind: presence, agg: ratio, bool_path: active, time_bucket: hour, render: timeline }\n\
             - { dashboard: Reliability, title: Crashes by PC, scope: fleet, kind: unexpected_shutdown, agg: count, group_by: pc_id, render: bar }\n\
             - { dashboard: Reliability, title: Total crashes, scope: fleet, kind: unexpected_shutdown, agg: count, render: stat }\n",
        );
        m.validate().expect("valid aggregate spec");
    }

    #[test]
    fn aggregate_rejects_empty_list() {
        let m = manifest_with_aggregate("aggregate: []\n");
        let err = m.validate().expect_err("empty list must fail");
        assert!(err.contains("at least one widget"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_ratio_without_bool_path() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: presence, agg: ratio, render: gauge }\n",
        );
        let err = m.validate().expect_err("ratio needs bool_path");
        assert!(err.contains("agg=ratio requires `bool_path`"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_sum_without_value_path() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: io, agg: sum, render: bar }\n",
        );
        let err = m.validate().expect_err("sum needs value_path");
        assert!(err.contains("agg=sum requires `value_path`"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_pc_id_group_without_fleet() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: presence, agg: count, group_by: pc_id, render: bar }\n",
        );
        let err = m.validate().expect_err("pc_id grouping needs fleet");
        assert!(
            err.contains("pc_id is only valid with scope: fleet"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_transform_with_pc_id_group() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, scope: fleet, kind: web_visit, agg: count, group_by: pc_id, transform: host, render: bar }\n",
        );
        let err = m
            .validate()
            .expect_err("transform on pc_id grouping must fail");
        assert!(
            err.contains("transform is not valid with group_by: pc_id"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_timeline_without_bucket() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: presence, agg: ratio, bool_path: active, render: timeline }\n",
        );
        let err = m.validate().expect_err("timeline needs a bucket");
        assert!(
            err.contains("render=timeline requires `time_bucket`"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_bucket_on_non_timeline() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: presence, agg: ratio, bool_path: active, time_bucket: hour, render: gauge }\n",
        );
        let err = m.validate().expect_err("bucket only on timeline");
        assert!(
            err.contains("time_bucket is only valid with render: timeline"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_unsafe_json_path() {
        // A path with characters outside [A-Za-z0-9_.] could break out of
        // the `'$.' || ?` bind — reject at create time.
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, group_by: \"foo'; DROP\", render: bar }\n",
        );
        let err = m.validate().expect_err("unsafe path must fail");
        assert!(err.contains("dotted JSON path"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_blank_title() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: \"  \", kind: k, agg: count, render: stat }\n",
        );
        let err = m.validate().expect_err("blank title must fail");
        assert!(err.contains("title must not be empty"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_blank_kind() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: \" \", agg: count, render: stat }\n",
        );
        let err = m.validate().expect_err("blank kind must fail");
        assert!(err.contains("kind must not be empty"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_blank_source_when_set() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, source: \"\", agg: count, render: stat }\n",
        );
        let err = m.validate().expect_err("blank source must fail");
        assert!(
            err.contains("source must not be empty when set"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_accepts_description_and_rejects_blank() {
        let ok = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, description: \"samples x 2 min\", kind: k, agg: count, render: stat }\n",
        );
        ok.validate()
            .expect("description is a valid optional field");
        assert_eq!(
            ok.aggregate.as_ref().unwrap()[0].description.as_deref(),
            Some("samples x 2 min")
        );
        let bad = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, description: \"  \", kind: k, agg: count, render: stat }\n",
        );
        let err = bad.validate().expect_err("blank description must fail");
        assert!(
            err.contains("description must not be empty when set"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_count_with_value_path() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, value_path: bytes, render: stat }\n",
        );
        let err = m.validate().expect_err("count must not use value_path");
        assert!(
            err.contains("agg=count does not use `value_path`"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_ratio_with_value_path() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: ratio, bool_path: active, value_path: bytes, render: gauge }\n",
        );
        let err = m.validate().expect_err("ratio must not use value_path");
        assert!(
            err.contains("agg=ratio does not use `value_path`"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_gauge_without_ratio() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, group_by: app, render: gauge }\n",
        );
        let err = m.validate().expect_err("gauge needs ratio");
        assert!(
            err.contains("render=gauge is only valid with agg: ratio"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_limit_without_group_by() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, limit: 5, render: stat }\n",
        );
        let err = m.validate().expect_err("limit needs group_by");
        assert!(err.contains("limit requires `group_by`"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_exclude_without_group_by() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, exclude: [x], render: stat }\n",
        );
        let err = m.validate().expect_err("exclude needs group_by");
        assert!(err.contains("exclude requires `group_by`"), "err: {err}");
    }

    #[test]
    fn aggregate_rejects_zero_limit_and_zero_sample_minutes() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, group_by: app, limit: 0, render: bar }\n",
        );
        assert!(m.validate().unwrap_err().contains("limit must be > 0"));
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, group_by: app, sample_minutes: 0, render: bar }\n",
        );
        assert!(
            m.validate()
                .unwrap_err()
                .contains("sample_minutes must be > 0")
        );
    }

    #[test]
    fn aggregate_rejects_empty_exclude_entry() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, group_by: app, exclude: [\"  \"], render: bar }\n",
        );
        let err = m.validate().expect_err("blank exclude entry must fail");
        assert!(
            err.contains("exclude must not contain empty entries"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_malformed_dotted_paths() {
        for bad in [".foo", "foo.", "foo..bar", "."] {
            let m = manifest_with_aggregate(&format!(
                "aggregate:\n- {{ dashboard: D, title: T, kind: k, agg: count, group_by: \"{bad}\", render: bar }}\n"
            ));
            let err = m.validate().expect_err("malformed path must fail");
            assert!(err.contains("dotted JSON path"), "path {bad}: {err}");
        }
    }

    #[test]
    fn aggregate_rejects_unknown_enum_value() {
        // An unrecognised render string deserialises to the #492 Unknown
        // catch-all (so old readers don't choke); validate() rejects it as
        // a typo at create time.
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, kind: k, agg: count, render: heatmap }\n",
        );
        let err = m.validate().expect_err("unknown render must fail");
        assert!(err.contains("render is not a known value"), "err: {err}");
    }

    #[test]
    fn aggregate_accepts_order_field() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: D, title: T, order: -5, kind: k, agg: count, render: stat }\n",
        );
        m.validate().expect("order is a valid optional field");
        let w = &m.aggregate.as_ref().unwrap()[0];
        assert_eq!(w.order, Some(-5));
    }

    #[test]
    fn aggregate_accepts_minimal_op_timeline() {
        // op_timeline needs no kind/agg — it reconstructs a fixed multi-kind
        // swimlane. A bare per-PC spec is valid, and `kind`/`agg` stay None.
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: Uptime, title: Operational state, scope: pc, render: op_timeline }\n",
        );
        m.validate().expect("minimal op_timeline is valid");
        let w = &m.aggregate.as_ref().unwrap()[0];
        assert_eq!(w.render, AggregateRender::OpTimeline);
        assert!(w.kind.is_none());
        assert!(w.agg.is_none());
    }

    #[test]
    fn aggregate_rejects_op_timeline_with_fleet_scope() {
        let m = manifest_with_aggregate(
            "aggregate:\n- { dashboard: Uptime, title: T, scope: fleet, render: op_timeline }\n",
        );
        let err = m.validate().expect_err("op_timeline must be per-PC");
        assert!(
            err.contains("render=op_timeline requires scope: pc"),
            "err: {err}"
        );
    }

    #[test]
    fn aggregate_rejects_op_timeline_with_aggregation_fields() {
        // Each aggregation knob the operator might paste in is rejected
        // (rather than silently ignored), pointing at the field to delete.
        for (block, field) in [
            ("kind: boot", "kind"),
            ("agg: count", "agg"),
            ("source: winlog:Security", "source"),
            ("group_by: pc_id", "group_by"),
            ("bool_path: active", "bool_path"),
            ("time_bucket: hour", "time_bucket"),
            ("limit: 5", "limit"),
        ] {
            let m = manifest_with_aggregate(&format!(
                "aggregate:\n- {{ dashboard: Uptime, title: T, scope: pc, {block}, render: op_timeline }}\n"
            ));
            let err = m
                .validate()
                .expect_err(&format!("op_timeline must reject {field}"));
            assert!(
                err.contains(&format!("render=op_timeline does not use `{field}`")),
                "field {field}: {err}"
            );
        }
    }

    // ── #743 View resource ───────────────────────────────────────────
    fn view_from(yaml_body: &str) -> View {
        serde_yaml::from_str(&format!("id: v1\n{yaml_body}")).expect("parse view")
    }

    #[test]
    fn view_accepts_valid_widgets() {
        let v = view_from(
            "widgets:\n\
             - { dashboard: Reliability, title: Crashes by PC, scope: fleet, kind: unexpected_shutdown, agg: count, group_by: pc_id, render: bar }\n\
             - { dashboard: Reliability, title: Total, scope: fleet, kind: unexpected_shutdown, agg: count, render: stat }\n",
        );
        v.validate().expect("valid view");
    }

    #[test]
    fn view_rejects_empty_widgets() {
        let v = view_from("widgets: []\n");
        let err = v.validate().expect_err("empty widgets must fail");
        assert!(err.contains("at least one widget"), "err: {err}");
    }

    #[test]
    fn view_rejects_blank_id() {
        let v: View = serde_yaml::from_str(
            "id: \"  \"\nwidgets:\n- { dashboard: D, title: T, kind: k, agg: count, render: stat }\n",
        )
        .expect("parse");
        let err = v.validate().expect_err("blank id must fail");
        assert!(err.contains("view.id must"), "err: {err}");
    }

    #[test]
    fn view_rejects_unsafe_id() {
        // A `/` or `..` in the id would break the KV key and the
        // `/api/views/{id}` URL segment — reject at create time.
        for bad in ["../etc", "a/b", "has space", "x;y"] {
            let v: View = serde_yaml::from_str(&format!(
                "id: \"{bad}\"\nwidgets:\n- {{ dashboard: D, title: T, kind: k, agg: count, render: stat }}\n",
            ))
            .expect("parse");
            let err = v.validate().expect_err("unsafe id must fail");
            assert!(err.contains("[A-Za-z0-9._-]"), "id {bad}: {err}");
        }
        assert!(is_valid_resource_id("dashboards-fleet.v1_2"));
    }

    #[test]
    fn view_reuses_shared_widget_validation() {
        // The same per-widget rule the job hint enforces (ratio needs
        // bool_path), reported under the `widgets[..]` field.
        let v = view_from(
            "widgets:\n- { dashboard: D, title: T, kind: presence, agg: ratio, render: gauge }\n",
        );
        let err = v.validate().expect_err("ratio without bool_path must fail");
        assert!(
            err.contains("widgets[0].agg=ratio requires `bool_path`"),
            "err: {err}"
        );
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
        // #492: the strict create boundary catches `script_objectt`
        // and similar fat-fingers (with the full path) instead of
        // letting them silently fall through to "all three unset".
        let yaml = r#"
id: typo
version: 1.0.0
execute:
  shell: powershell
  script_objectt: oops
  timeout: 30s
"#;
        let err = crate::strict::from_yaml_str::<Manifest>(yaml)
            .expect_err("typo'd execute field must be rejected at the write boundary");
        assert!(err.contains("execute.script_objectt"), "{err}");
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

    #[test]
    fn schedule_tags_default_empty_and_skip_serialise() {
        let yaml = r#"
id: x
when:
  per_pc: once
job_id: y
target: { all: true }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert!(s.tags.is_empty());
        s.validate().expect("tag-less schedule validates");
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(
            !json.contains("tags"),
            "empty tags must not serialise: {json}"
        );
    }

    #[test]
    fn schedule_parses_and_validates_tags() {
        let yaml = r#"
id: weekly-cleanup
when:
  per_pc: { every: 1h }
job_id: cleanup
target: { all: true }
tags: [weekly, maintenance]
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.tags, vec!["weekly", "maintenance"]);
        s.validate().expect("tagged schedule validates");
    }

    #[test]
    fn schedule_rejects_blank_tag() {
        let yaml = r#"
id: x
when:
  per_pc: once
job_id: y
target: { all: true }
tags: [ok, "  "]
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        let err = s.validate().expect_err("blank tag must fail");
        assert!(err.contains("tags must not contain empty"), "err: {err}");
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
        // `{ evry: 6h }` still fails on the tolerant read path: the
        // required `every` key is missing, so no PerPolicy variant
        // matches (#492 removed deny_unknown_fields, but required
        // keys keep the untagged disambiguation honest).
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
            When::On(vec![OnTrigger::Startup]),
            When::On(vec![OnTrigger::Startup, OnTrigger::Logon]),
            When::On(vec![OnTrigger::Lock, OnTrigger::Unlock]),
            When::On(vec![OnTrigger::NetworkChange]),
        ] {
            // Event triggers are agent-only; the rest validate on backend.
            let runs_on = if matches!(when, When::On(_)) {
                RunsOn::Agent
            } else {
                RunsOn::Backend
            };
            let s = schedule_with(when.clone(), runs_on);

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
            (When::On(vec![OnTrigger::Startup]), "on [startup]"),
            (
                When::On(vec![OnTrigger::Startup, OnTrigger::Logon]),
                "on [startup,logon]",
            ),
            (
                When::On(vec![OnTrigger::Lock, OnTrigger::Unlock]),
                "on [lock,unlock]",
            ),
            (
                When::On(vec![OnTrigger::NetworkChange]),
                "on [network_change]",
            ),
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
            constraints: Constraints::default(),
            on_failure: OnFailure::default(),
            tz: ScheduleTz::default(),
            starting_deadline: None,
            runs_on,
            enabled: true,
            tags: Vec::new(),
            origin: None,
        }
    }

    fn calendar(at: &str, days: &[&str]) -> When {
        When::Calendar(CalendarSpec {
            at: at.into(),
            days: days.iter().map(|d| (*d).to_string()).collect(),
        })
    }

    #[test]
    fn next_calendar_fire_returns_next_utc_occurrence() {
        use chrono::TimeZone;
        // Daily 09:00, evaluated in UTC. From 08:00 the same day, the
        // next strict occurrence is 09:00 that day.
        let mut s = schedule_with(calendar("09:00", &[]), RunsOn::Backend);
        s.tz = ScheduleTz::Utc;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 9, 8, 0, 0).unwrap();
        let next = s.next_calendar_fire(now).expect("calendar has a next fire");
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2026, 6, 9, 9, 0, 0).unwrap()
        );
    }

    #[test]
    fn next_calendar_fire_is_strictly_after_now() {
        use chrono::TimeZone;
        // Standing exactly on a fire instant must preview the *next*
        // one (inclusive = false), not the one firing right now.
        let mut s = schedule_with(calendar("09:00", &[]), RunsOn::Backend);
        s.tz = ScheduleTz::Utc;
        let on_fire = chrono::Utc.with_ymd_and_hms(2026, 6, 9, 9, 0, 0).unwrap();
        let next = s
            .next_calendar_fire(on_fire)
            .expect("calendar has a next fire");
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap()
        );
    }

    #[test]
    fn next_calendar_fire_none_for_reconcile_shapes() {
        // `per_pc` / `per_target` lower to the every-minute poll cron —
        // no discrete upcoming event to preview, so `None`.
        let now = chrono::Utc::now();
        for when in [
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            When::PerTarget(PerPolicy::Once(OnceLiteral::Once)),
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            When::PerTarget(PerPolicy::Every(EverySpec {
                every: "24h".into(),
            })),
        ] {
            let s = schedule_with(when, RunsOn::Backend);
            assert!(
                s.next_calendar_fire(now).is_none(),
                "reconcile shapes have no calendar fire",
            );
        }
    }

    // ---- preview_fires (#418 dry-run / preview) ----

    fn cal_utc(at: &str, days: &[&str]) -> Schedule {
        let mut s = schedule_with(calendar(at, days), RunsOn::Backend);
        s.tz = ScheduleTz::Utc; // host-independent assertions
        s
    }

    #[test]
    fn preview_lists_next_calendar_occurrences() {
        use chrono::TimeZone;
        // Weekday 09:00, from Wed 2026-06-10 00:00 UTC: the next five
        // fires skip the weekend (Sat 13 / Sun 14).
        let s = cal_utc("09:00", &["mon-fri"]);
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 10, 0, 0, 0).unwrap();
        let got = s.preview_fires(now, 5);
        let want: Vec<_> = [
            (2026, 6, 10), // Wed
            (2026, 6, 11), // Thu
            (2026, 6, 12), // Fri
            (2026, 6, 15), // Mon (skips Sat 13 / Sun 14)
            (2026, 6, 16), // Tue
        ]
        .iter()
        .map(|(y, m, d)| chrono::Utc.with_ymd_and_hms(*y, *m, *d, 9, 0, 0).unwrap())
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn preview_handles_nth_and_last_weekday() {
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        // 2nd Tuesday (Patch Tuesday): Jun 9, Jul 14 2026.
        let nth = cal_utc("09:00", &["tue#2"]).preview_fires(now, 2);
        assert_eq!(
            nth,
            vec![
                chrono::Utc.with_ymd_and_hms(2026, 6, 9, 9, 0, 0).unwrap(),
                chrono::Utc.with_ymd_and_hms(2026, 7, 14, 9, 0, 0).unwrap(),
            ]
        );
        // Last Friday of the month: Jun 26, Jul 31 2026.
        let last = cal_utc("22:00", &["friL"]).preview_fires(now, 2);
        assert_eq!(
            last,
            vec![
                chrono::Utc.with_ymd_and_hms(2026, 6, 26, 22, 0, 0).unwrap(),
                chrono::Utc.with_ymd_and_hms(2026, 7, 31, 22, 0, 0).unwrap(),
            ]
        );
    }

    #[test]
    fn preview_is_empty_for_reconcile_and_zero_count() {
        let now = chrono::Utc::now();
        // reconcile shapes have no discrete fire times
        let recon = schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            RunsOn::Backend,
        );
        assert!(recon.preview_fires(now, 5).is_empty());
        // count == 0 yields nothing even for a calendar
        assert!(cal_utc("09:00", &[]).preview_fires(now, 0).is_empty());
    }

    #[test]
    fn preview_skips_outside_active_window() {
        use chrono::TimeZone;
        // Daily 09:00, active only [2026-06-15, 2026-06-17). Occurrences
        // before `from` are skipped; `until` is exclusive, so 06-17's
        // fire is out — leaving exactly the 15th and 16th.
        let mut s = cal_utc("09:00", &[]);
        s.active = Active {
            from: Some("2026-06-15".into()),
            until: Some("2026-06-17".into()),
        };
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 10, 0, 0, 0).unwrap();
        let got = s.preview_fires(now, 5);
        assert_eq!(
            got,
            vec![
                chrono::Utc.with_ymd_and_hms(2026, 6, 15, 9, 0, 0).unwrap(),
                chrono::Utc.with_ymd_and_hms(2026, 6, 16, 9, 0, 0).unwrap(),
            ]
        );
    }

    #[test]
    fn preview_empty_when_calendar_time_outside_window() {
        use chrono::TimeZone;
        // Fires at 09:00 but the maintenance window is overnight — it can
        // never run, so the preview is empty (matches
        // `calendar_outside_window`), and the scan still terminates.
        let mut s = cal_utc("09:00", &[]);
        s.constraints = Constraints {
            window: Some("22:00-05:00".into()),
            ..Constraints::default()
        };
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 10, 0, 0, 0).unwrap();
        assert!(s.preview_fires(now, 5).is_empty());
        // Every candidate tick is rejected, so this also exercises the
        // SCAN_CAP bound: a large `count` must still terminate (and
        // return empty) rather than spin (claude #578 review).
        assert!(s.preview_fires(now, 50).is_empty());
    }

    #[test]
    fn preview_past_one_shot_is_empty() {
        use chrono::TimeZone;
        // A dated one-shot whose instant has passed never fires again.
        let s = cal_utc("2026-06-10 09:00", &[]);
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap();
        assert!(s.preview_fires(now, 5).is_empty());
        // …but from before it, the single future fire shows up.
        let before = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        assert_eq!(
            s.preview_fires(before, 5),
            vec![chrono::Utc.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap()]
        );
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

    // ---- #418 event triggers (when: { on }) ----

    #[test]
    fn validate_accepts_event_on_agent() {
        for triggers in [
            vec![OnTrigger::Startup],
            vec![OnTrigger::Logon],
            vec![OnTrigger::Lock],
            vec![OnTrigger::Unlock],
            vec![OnTrigger::NetworkChange],
            vec![
                OnTrigger::Startup,
                OnTrigger::Logon,
                OnTrigger::Lock,
                OnTrigger::Unlock,
                OnTrigger::NetworkChange,
            ],
        ] {
            schedule_with(When::On(triggers), RunsOn::Agent)
                .validate()
                .expect("when.on is valid on runs_on: agent");
        }
    }

    #[test]
    fn validate_rejects_event_on_backend() {
        let err = schedule_with(When::On(vec![OnTrigger::Startup]), RunsOn::Backend)
            .validate()
            .unwrap_err();
        assert!(err.contains("when.on"), "got: {err}");
        assert!(err.contains("runs_on: agent"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_event_list() {
        let err = schedule_with(When::On(vec![]), RunsOn::Agent)
            .validate()
            .unwrap_err();
        assert!(err.contains("when.on"), "got: {err}");
        assert!(err.contains("at least one"), "got: {err}");
    }

    #[test]
    fn event_schedule_lowers_to_event_mode_and_is_event() {
        let s = schedule_with(When::On(vec![OnTrigger::Startup]), RunsOn::Agent);
        assert!(s.is_event());
        assert_eq!(s.lowered().mode, ExecMode::Event);
        assert_eq!(s.event_triggers(), &[OnTrigger::Startup]);
        // non-event schedules report no triggers.
        let cal = schedule_with(calendar("09:00", &[]), RunsOn::Backend);
        assert!(!cal.is_event());
        assert!(cal.event_triggers().is_empty());
    }

    // ---- #418 constraints.require (env gates) ----

    fn require_schedule(req: Require, runs_on: RunsOn) -> Schedule {
        let mut s = schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "1m".into() })),
            runs_on,
        );
        s.constraints.require = Some(req);
        s
    }

    #[test]
    fn require_met_combinations() {
        use std::time::Duration;
        let idle = |m: u64| Some(Duration::from_secs(m * 60));
        // Builder for the sensed state: (ac, idle, cpu, network).
        let env = |ac, idle, cpu, net| EnvState {
            ac_online: ac,
            idle,
            cpu_pct: cpu,
            network_up: net,
        };
        // Empty require — always met regardless of sensed state.
        assert!(require_met(
            &Require::default(),
            &env(false, None, None, false)
        ));
        // ac_power: only on AC.
        let ac = Require {
            ac_power: true,
            ..Default::default()
        };
        assert!(!require_met(&ac, &env(false, None, None, true)));
        assert!(require_met(&ac, &env(true, None, None, false)));
        // idle: needs >= the configured min; None idle never satisfies.
        let idle10 = Require {
            idle: Some("10m".into()),
            ..Default::default()
        };
        assert!(!require_met(&idle10, &env(true, None, None, true)));
        assert!(!require_met(&idle10, &env(true, idle(5), None, true)));
        assert!(require_met(&idle10, &env(true, idle(15), None, true)));
        assert!(require_met(&idle10, &env(true, idle(10), None, true))); // boundary inclusive
        // cpu_below: needs CPU strictly < threshold; None cpu never satisfies.
        let cpu20 = Require {
            cpu_below: Some(20.0),
            ..Default::default()
        };
        assert!(!require_met(&cpu20, &env(true, None, None, true))); // no sample → fail-closed
        assert!(!require_met(&cpu20, &env(true, None, Some(20.0), true))); // == threshold
        assert!(!require_met(&cpu20, &env(true, None, Some(55.0), true))); // busy
        assert!(require_met(&cpu20, &env(true, None, Some(5.0), true))); // quiet
        // network: only when online.
        let net = Require {
            network: true,
            ..Default::default()
        };
        assert!(!require_met(&net, &env(true, None, None, false))); // offline
        assert!(require_met(&net, &env(true, None, None, true))); // online
        // all four: AND.
        let all = Require {
            ac_power: true,
            idle: Some("10m".into()),
            cpu_below: Some(20.0),
            network: true,
        };
        assert!(!require_met(&all, &env(false, idle(20), Some(5.0), true))); // on battery
        assert!(!require_met(&all, &env(true, idle(1), Some(5.0), true))); // not idle enough
        assert!(!require_met(&all, &env(true, idle(20), Some(50.0), true))); // busy
        assert!(!require_met(&all, &env(true, idle(20), Some(5.0), false))); // offline
        assert!(require_met(&all, &env(true, idle(20), Some(5.0), true)));
        // An unparseable idle is treated as no-requirement by require_met
        // (validate rejects it at create time, so this only guards a
        // hand-edited blob): ac still gates.
        let bad = Require {
            ac_power: true,
            idle: Some("garbage".into()),
            ..Default::default()
        };
        assert!(require_met(&bad, &env(true, None, None, true)));
        assert!(!require_met(&bad, &env(false, None, None, true)));
    }

    #[test]
    fn validate_accepts_and_rejects_cpu_below() {
        // In-range accepted.
        require_schedule(
            Require {
                cpu_below: Some(20.0),
                ..Default::default()
            },
            RunsOn::Agent,
        )
        .validate()
        .expect("cpu_below 20 is valid");
        // Upper boundary: 100.0 is accepted (fires unless CPU is exactly
        // 100%). Pins the inclusive upper bound against a future c < 100.0.
        require_schedule(
            Require {
                cpu_below: Some(100.0),
                ..Default::default()
            },
            RunsOn::Agent,
        )
        .validate()
        .expect("cpu_below 100 is valid");
        // Out of range rejected (0 and >100).
        for bad in [0.0, -5.0, 100.1] {
            let err = require_schedule(
                Require {
                    cpu_below: Some(bad),
                    ..Default::default()
                },
                RunsOn::Agent,
            )
            .validate()
            .unwrap_err();
            assert!(
                err.contains("constraints.require.cpu_below"),
                "cpu_below {bad}: {err}"
            );
        }
    }

    #[test]
    fn validate_accepts_require_on_agent() {
        require_schedule(
            Require {
                ac_power: true,
                idle: Some("10m".into()),
                cpu_below: Some(20.0),
                network: true,
            },
            RunsOn::Agent,
        )
        .validate()
        .expect("constraints.require is valid on runs_on: agent");
    }

    #[test]
    fn validate_rejects_require_on_backend() {
        let err = require_schedule(
            Require {
                ac_power: true,
                ..Default::default()
            },
            RunsOn::Backend,
        )
        .validate()
        .unwrap_err();
        assert!(err.contains("constraints.require"), "got: {err}");
        assert!(err.contains("runs_on: agent"), "got: {err}");

        // An idle-only require (ac_power: false) is also non-empty
        // (is_empty folds the fields) and must reject on backend too —
        // guards against a regression in Require::is_empty.
        let err = require_schedule(
            Require {
                idle: Some("10m".into()),
                ..Default::default()
            },
            RunsOn::Backend,
        )
        .validate()
        .unwrap_err();
        assert!(
            err.contains("constraints.require"),
            "idle-only on backend: {err}"
        );
    }

    #[test]
    fn validate_rejects_bad_require_idle() {
        let err = require_schedule(
            Require {
                idle: Some("not-a-duration".into()),
                ..Default::default()
            },
            RunsOn::Agent,
        )
        .validate()
        .unwrap_err();
        assert!(err.contains("constraints.require.idle"), "got: {err}");
    }

    #[test]
    fn require_round_trips_and_skips_empty() {
        // ac_power: false is skipped; an all-default require nested in
        // constraints is omitted (is_empty folds it in).
        let yaml = "id: s\nwhen: { per_pc: { every: 1m } }\njob_id: j\nruns_on: agent\n\
                    constraints: { require: { ac_power: true, idle: 10m, cpu_below: 20, \
                    network: true } }\n";
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        let req = s.constraints.require.as_ref().expect("require present");
        assert!(req.ac_power);
        assert_eq!(req.idle.as_deref(), Some("10m"));
        assert_eq!(req.cpu_below, Some(20.0));
        assert!(req.network);
        // Re-serialize: idle + cpu_below + network present, ac_power true.
        let back = serde_json::to_string(&s.constraints).unwrap();
        assert!(back.contains("\"idle\":\"10m\""), "got: {back}");
        assert!(back.contains("\"cpu_below\":20"), "got: {back}");
        assert!(back.contains("\"network\":true"), "got: {back}");
        // An empty require is omitted entirely by is_empty.
        let mut empty = s.clone();
        empty.constraints.require = Some(Require::default());
        assert!(empty.constraints.is_empty());
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
        // a degenerate range like `mon-` reports the whole token, not
        // a cryptic empty part (claude #432 follow-up)
        let err = schedule_with(calendar("09:00", &["mon-"]), RunsOn::Backend)
            .validate()
            .unwrap_err();
        assert!(err.contains("'mon-'"), "names the whole token: {err}");
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
    fn validate_accepts_nth_weekday() {
        // #418: nth-weekday (Patch Tuesday). validate() also lowers to
        // a cron and parses it with croner, so passing here proves the
        // whole chain — token → DOW field → engine-acceptable cron.
        for ok in [
            calendar("09:00", &["tue#2"]),          // 2nd Tuesday
            calendar("09:00", &["fri#1"]),          // 1st Friday
            calendar("03:00", &["sun#5"]),          // 5th Sunday
            calendar("09:00", &["tue#2", "thu#2"]), // a list of nths
            calendar("09:00", &["2#2"]),            // numeric DOW + ordinal
            // Case-insensitive both sides: validate lowercases, croner
            // upper-cases the whole pattern before aliasing (claude #547).
            calendar("09:00", &["TUE#2"]),
        ] {
            schedule_with(ok.clone(), RunsOn::Backend)
                .validate()
                .unwrap_or_else(|e| panic!("{ok} should validate: {e}"));
        }
    }

    #[test]
    fn validate_rejects_bad_nth_weekday() {
        // ordinal out of 1..5, a range with #, and a bad day before #.
        for bad in ["tue#0", "tue#6", "tue#x", "mon-fri#2", "funday#2"] {
            let err = schedule_with(calendar("09:00", &[bad]), RunsOn::Backend)
                .validate()
                .unwrap_err();
            assert!(err.contains("when.days"), "for '{bad}', got: {err}");
        }
    }

    #[test]
    fn validate_accepts_last_weekday() {
        // #418: last-weekday (`friL` = last Friday). Like the nth case,
        // validate() lowers to a cron and round-trips it through croner,
        // so passing proves token → DOW field → engine-acceptable cron
        // with the verified last-<dow>-of-month semantics.
        for ok in [
            calendar("09:00", &["friL"]),         // last Friday
            calendar("03:00", &["sunL"]),         // last Sunday
            calendar("22:00", &["5L"]),           // numeric DOW + last
            calendar("00:00", &["0L"]),           // numeric Sunday (0…
            calendar("00:00", &["7L"]),           // …and its 7 alias)
            calendar("09:00", &["monL", "friL"]), // a list of last-weekdays
            // Case-insensitive both the weekday and the `L` suffix:
            // validate lowercases the day, croner upper-cases the whole
            // pattern before aliasing (claude #547).
            calendar("09:00", &["FRIL"]),
            calendar("09:00", &["fril"]),
        ] {
            schedule_with(ok.clone(), RunsOn::Backend)
                .validate()
                .unwrap_or_else(|e| panic!("{ok} should validate: {e}"));
        }
    }

    #[test]
    fn validate_rejects_bad_last_weekday() {
        // bare `L` (no weekday — a footgun croner reads as Saturday), a
        // range with L, a bad day before L, and an internal space that
        // would otherwise leak a malformed cron downstream (gemini #560).
        for bad in ["L", "l", "mon-friL", "fundayL", "8L", "*L", "fri L"] {
            let err = schedule_with(calendar("09:00", &[bad]), RunsOn::Backend)
                .validate()
                .unwrap_err();
            assert!(err.contains("when.days"), "for '{bad}', got: {err}");
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

    // ---- constraints.window (#418 Phase 3) ----

    fn with_window(win: &str) -> Schedule {
        let mut s = schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            RunsOn::Backend,
        );
        s.constraints.window = Some(win.into());
        s
    }

    #[test]
    fn constraints_window_parses_and_round_trips() {
        let yaml = r#"
id: x
when:
  per_pc: { every: 6h }
job_id: y
target: { all: true }
constraints:
  window: "22:00-05:00"
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(s.constraints.window.as_deref(), Some("22:00-05:00"));
        let back: Schedule =
            serde_json::from_str(&serde_json::to_string(&s).expect("ser")).expect("de");
        assert_eq!(back.constraints.window.as_deref(), Some("22:00-05:00"));
    }

    #[test]
    fn constraints_empty_is_skipped_when_serialising() {
        let s = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        let json = serde_json::to_value(&s).expect("serialise");
        assert!(
            json.get("constraints").is_none(),
            "empty constraints must not appear on the wire: {json}"
        );
    }

    #[test]
    fn window_no_constraint_always_allows() {
        let c = Constraints::default();
        assert!(c.allows(chrono::Utc::now(), ScheduleTz::Local));
    }

    #[test]
    fn window_same_day_is_half_open() {
        use chrono::TimeZone;
        let s = with_window("09:00-17:00");
        let at = |h, m| chrono::Utc.with_ymd_and_hms(2026, 6, 9, h, m, 0).unwrap();
        let a = |t| s.constraints.allows(t, ScheduleTz::Utc);
        assert!(!a(at(8, 59)), "before start");
        assert!(a(at(9, 0)), "at start (inclusive)");
        assert!(a(at(16, 59)), "inside");
        assert!(!a(at(17, 0)), "at end (exclusive)");
        assert!(!a(at(23, 0)), "after end");
    }

    #[test]
    fn window_crossing_midnight() {
        use chrono::TimeZone;
        let s = with_window("22:00-05:00");
        let at = |h, m| chrono::Utc.with_ymd_and_hms(2026, 6, 9, h, m, 0).unwrap();
        let a = |t| s.constraints.allows(t, ScheduleTz::Utc);
        assert!(a(at(22, 0)), "at start tonight");
        assert!(a(at(23, 30)), "late tonight");
        assert!(a(at(3, 0)), "early tomorrow");
        assert!(!a(at(5, 0)), "at end (exclusive)");
        assert!(!a(at(12, 0)), "midday outside");
        assert!(!a(at(21, 59)), "just before start");
    }

    #[test]
    fn window_respects_tz() {
        // The same instant is inside the window under one tz and may
        // be outside under another. Compare UTC vs Local via the
        // host's own offset (kept CI-green on UTC runners like the
        // active tz test does).
        use chrono::TimeZone;
        let s = with_window("09:00-17:00");
        let noon_utc = chrono::Utc.with_ymd_and_hms(2026, 6, 9, 12, 0, 0).unwrap();
        // Under UTC, 12:00 is inside 09:00-17:00.
        assert!(s.constraints.allows(noon_utc, ScheduleTz::Utc));
        // Under Local, the verdict tracks the host wall-clock time;
        // assert it matches a direct wall_time membership check.
        let local_t = noon_utc.with_timezone(&chrono::Local).time();
        let in_local = local_t >= chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap()
            && local_t < chrono::NaiveTime::from_hms_opt(17, 0, 0).unwrap();
        assert_eq!(s.constraints.allows(noon_utc, ScheduleTz::Local), in_local);
    }

    #[test]
    fn validate_accepts_good_window() {
        for w in ["09:00-17:00", "22:00-05:00", "00:00-23:59"] {
            with_window(w)
                .validate()
                .unwrap_or_else(|e| panic!("'{w}' should validate: {e}"));
        }
    }

    #[test]
    fn validate_rejects_bad_window() {
        for bad in ["9-5", "22:00", "22:00-22:00", "25:00-05:00", "09:00_17:00"] {
            let err = with_window(bad).validate().unwrap_err();
            assert!(
                err.contains("constraints.window"),
                "for '{bad}', got: {err}"
            );
        }
    }

    // ---- constraints.skip_dates (#418 holiday exclusion) ----

    fn with_skip_dates(dates: &[&str]) -> Schedule {
        let mut s = schedule_with(calendar("09:00", &[]), RunsOn::Backend);
        s.tz = ScheduleTz::Utc; // host-independent date assertions
        s.constraints.skip_dates = dates.iter().map(|d| (*d).to_string()).collect();
        s
    }

    #[test]
    fn allows_blocks_listed_skip_date() {
        use chrono::TimeZone;
        let s = with_skip_dates(&["2026-06-10", "2026-12-25"]);
        // Any time on a listed date is blocked (whole day).
        let on = chrono::Utc.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap();
        assert!(!s.constraints.allows(on, ScheduleTz::Utc));
        let on_midnight = chrono::Utc.with_ymd_and_hms(2026, 12, 25, 0, 0, 0).unwrap();
        assert!(!s.constraints.allows(on_midnight, ScheduleTz::Utc));
        // A date not in the list fires normally.
        let off = chrono::Utc.with_ymd_and_hms(2026, 6, 11, 9, 0, 0).unwrap();
        assert!(s.constraints.allows(off, ScheduleTz::Utc));
    }

    #[test]
    fn allows_corrupt_skip_date_fails_closed() {
        use chrono::TimeZone;
        // A garbled entry (only reachable via hand-edited KV) blocks
        // rather than silently re-enabling fires — same posture as a
        // corrupt window.
        let s = with_skip_dates(&["not-a-date"]);
        let any = chrono::Utc.with_ymd_and_hms(2026, 6, 11, 9, 0, 0).unwrap();
        assert!(!s.constraints.allows(any, ScheduleTz::Utc));
    }

    #[test]
    fn validate_accepts_good_skip_dates() {
        with_skip_dates(&["2026-01-01", "2026-12-25", "2027-05-03"])
            .validate()
            .expect("well-formed skip dates should validate");
    }

    #[test]
    fn validate_rejects_bad_skip_date() {
        for bad in ["2026-13-01", "01-01-2026", "nope", "2026/01/01"] {
            let err = with_skip_dates(&[bad]).validate().unwrap_err();
            assert!(
                err.contains("constraints.skip_dates"),
                "for '{bad}', got: {err}"
            );
        }
    }

    #[test]
    fn preview_skips_holidays() {
        use chrono::TimeZone;
        // Daily 09:00 with two of the next five days marked as holidays
        // — preview drops exactly those, since it gates on `allows`.
        let mut s = cal_utc("09:00", &[]);
        s.constraints.skip_dates = vec!["2026-06-11".into(), "2026-06-13".into()];
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 10, 0, 0, 0).unwrap();
        let got = s.preview_fires(now, 4);
        let want: Vec<_> = [
            (2026, 6, 10),
            (2026, 6, 12), // skips 06-11
            (2026, 6, 14), // skips 06-13
            (2026, 6, 15),
        ]
        .iter()
        .map(|(y, m, d)| chrono::Utc.with_ymd_and_hms(*y, *m, *d, 9, 0, 0).unwrap())
        .collect();
        assert_eq!(got, want);
    }

    // ---- constraints.max_concurrent (#418) ----

    fn with_max_concurrent(max: u32, runs_on: RunsOn) -> Schedule {
        let mut s = schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            runs_on,
        );
        s.constraints.max_concurrent = Some(max);
        s
    }

    #[test]
    fn validate_accepts_backend_max_concurrent() {
        with_max_concurrent(5, RunsOn::Backend)
            .validate()
            .expect("backend max_concurrent should validate");
    }

    #[test]
    fn validate_rejects_max_concurrent_on_agent() {
        // Decision E: a central running-instance cap needs a central
        // counter, which agents don't have.
        let err = with_max_concurrent(5, RunsOn::Agent)
            .validate()
            .unwrap_err();
        assert!(err.contains("constraints.max_concurrent"), "got: {err}");
        assert!(err.contains("runs_on: agent"), "got: {err}");
    }

    #[test]
    fn validate_rejects_zero_max_concurrent() {
        let err = with_max_concurrent(0, RunsOn::Backend)
            .validate()
            .unwrap_err();
        assert!(err.contains("max_concurrent must be >= 1"), "got: {err}");
    }

    #[test]
    fn max_concurrent_round_trips_and_skips_when_absent() {
        let s = with_max_concurrent(3, RunsOn::Backend);
        let json = serde_json::to_value(&s.constraints).expect("ser");
        assert_eq!(json.get("max_concurrent").and_then(|v| v.as_u64()), Some(3));
        // A schedule with no constraints omits the whole block.
        let bare = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        assert!(bare.constraints.is_empty());
    }

    #[test]
    fn window_fail_closed_on_corrupt_blob() {
        // A malformed window (only reachable via a hand-edited KV
        // blob — validate() rejects it at create) must BLOCK, not
        // silently allow fires during a change-freeze (gemini #452).
        let s = with_window("22:00_05:00");
        assert!(
            !s.constraints.allows(chrono::Utc::now(), ScheduleTz::Utc),
            "corrupt window fails closed"
        );
        // …and the scheduler can surface why it's stuck.
        assert!(
            s.bad_window().is_some(),
            "bad_window reports the parse error"
        );
        assert!(with_window("22:00-05:00").bad_window().is_none());
    }

    #[test]
    fn calendar_outside_window_is_flagged() {
        // at 09:00 can never fall in 22:00-05:00 → never fires.
        let mut s = schedule_with(calendar("09:00", &["mon-fri"]), RunsOn::Backend);
        s.constraints.window = Some("22:00-05:00".into());
        assert!(s.calendar_outside_window(), "09:00 is not in 22:00-05:00");

        // at 23:00 IS inside the overnight window → fine.
        let mut s = schedule_with(calendar("23:00", &[]), RunsOn::Backend);
        s.constraints.window = Some("22:00-05:00".into());
        assert!(!s.calendar_outside_window(), "23:00 is in 22:00-05:00");

        // reconcile shapes are never flagged (they poll every minute).
        let mut s = schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            RunsOn::Backend,
        );
        s.constraints.window = Some("22:00-05:00".into());
        assert!(!s.calendar_outside_window(), "reconcile is unaffected");

        // no window → never flagged.
        let s = schedule_with(calendar("09:00", &[]), RunsOn::Backend);
        assert!(!s.calendar_outside_window());
    }

    // ---- on_failure.retry (#418 Phase 4) ----

    fn with_retry(max: u32, backoff: &str) -> Schedule {
        let mut s = schedule_with(
            When::PerPc(PerPolicy::Every(EverySpec { every: "6h".into() })),
            RunsOn::Backend,
        );
        s.on_failure.retry = Some(Retry {
            max,
            backoff: backoff.into(),
        });
        s
    }

    #[test]
    fn on_failure_parses_and_round_trips() {
        let yaml = r#"
id: x
when:
  per_pc: { every: 6h }
job_id: y
target: { all: true }
on_failure:
  retry: { max: 3, backoff: 10m }
"#;
        let s: Schedule = serde_yaml::from_str(yaml).expect("parse");
        let r = s.on_failure.retry.as_ref().expect("retry present");
        assert_eq!(r.max, 3);
        assert_eq!(r.backoff, "10m");
        let back: Schedule =
            serde_json::from_str(&serde_json::to_string(&s).expect("ser")).expect("de");
        assert_eq!(back.on_failure, s.on_failure);
    }

    #[test]
    fn on_failure_empty_is_skipped_when_serialising() {
        let s = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        let json = serde_json::to_value(&s).expect("serialise");
        assert!(
            json.get("on_failure").is_none(),
            "empty on_failure must not appear on the wire: {json}"
        );
    }

    #[test]
    fn validate_accepts_good_retry() {
        for (max, backoff) in [(1, "30s"), (3, "10m"), (10, "1h")] {
            with_retry(max, backoff)
                .validate()
                .unwrap_or_else(|e| panic!("retry {{max:{max}, backoff:{backoff}}}: {e}"));
        }
    }

    #[test]
    fn validate_rejects_bad_backoff() {
        let err = with_retry(3, "soon").validate().unwrap_err();
        assert!(err.contains("on_failure.retry.backoff"), "got: {err}");
    }

    #[test]
    fn validate_rejects_sub_second_backoff() {
        // "500ms" parses as humantime but lowers to 0s on the wire —
        // reject it so the operator doesn't get a silent no-wait
        // (coderabbit #466).
        for bad in ["500ms", "0s", "999ms"] {
            let err = with_retry(3, bad).validate().unwrap_err();
            assert!(
                err.contains("on_failure.retry.backoff must be >= 1s"),
                "for '{bad}', got: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_out_of_range_max() {
        for bad in [0u32, 11, 1000] {
            let err = with_retry(bad, "10m").validate().unwrap_err();
            assert!(
                err.contains("on_failure.retry.max"),
                "for max={bad}, got: {err}"
            );
        }
    }

    #[test]
    fn lowered_retry_reduces_backoff_to_seconds() {
        let s = with_retry(3, "10m");
        let spec = s.on_failure.lowered_retry().expect("a retry policy");
        assert_eq!(spec.max, 3);
        assert_eq!(spec.backoff_secs, 600);
    }

    #[test]
    fn lowered_retry_is_none_without_policy() {
        let s = schedule_with(
            When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            RunsOn::Backend,
        );
        assert!(s.on_failure.lowered_retry().is_none());
    }

    // ---- global change-freeze (#418 Phase 5) ----

    #[test]
    fn freeze_empty_window_is_always_active() {
        // The big-red-button shape: no bounds = frozen until cleared.
        let f = Freeze::default();
        assert!(f.is_active(chrono::Utc::now()));
    }

    #[test]
    fn freeze_window_is_half_open() {
        use chrono::TimeZone;
        let f = Freeze {
            from: Some("2026-12-20T00:00:00+00:00".into()),
            until: Some("2027-01-05T00:00:00+00:00".into()),
            reason: Some("year-end".into()),
            tz: ScheduleTz::Utc,
        };
        let at = |y, mo, d| chrono::Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap();
        assert!(!f.is_active(at(2026, 12, 19)), "before from = not frozen");
        assert!(f.is_active(at(2026, 12, 20)), "from is inclusive");
        assert!(f.is_active(at(2026, 12, 31)), "inside window");
        assert!(!f.is_active(at(2027, 1, 5)), "until is exclusive");
        assert!(!f.is_active(at(2027, 1, 6)), "after until = not frozen");
    }

    #[test]
    fn freeze_fails_closed_on_corrupt_bound() {
        // A freeze is a safety switch: an unparseable bound (only
        // reachable via a hand-edited KV blob) must read as FROZEN, not
        // "fire normally" (coderabbit #472) — the opposite of `active`,
        // which fail-opens.
        let f = Freeze {
            from: Some("not-a-date".into()),
            until: None,
            reason: None,
            tz: ScheduleTz::Utc,
        };
        assert!(f.is_active(chrono::Utc::now()), "corrupt bound → frozen");
    }

    #[test]
    fn freeze_validate_accepts_good_bounds() {
        Freeze {
            from: Some("2026-12-20".into()),
            until: Some("2027-01-05T12:00:00+09:00".into()),
            reason: None,
            tz: ScheduleTz::Local,
        }
        .validate()
        .expect("date + rfc3339 bounds should validate");
        // Empty (indefinite) freeze is valid.
        Freeze::default().validate().expect("empty freeze is valid");
    }

    #[test]
    fn freeze_validate_rejects_bad_bound_and_inverted_window() {
        let err = Freeze {
            from: Some("never".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("freeze:"), "got: {err}");

        let inverted = Freeze {
            from: Some("2027-01-05".into()),
            until: Some("2026-12-20".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(inverted.contains("freeze.from"), "got: {inverted}");
    }

    #[test]
    fn freeze_round_trips_and_skips_empty_fields() {
        let f = Freeze {
            from: None,
            until: Some("2027-01-05".into()),
            reason: Some("INC-1234".into()),
            tz: ScheduleTz::Utc,
        };
        let json = serde_json::to_value(&f).expect("serialise");
        assert!(json.get("from").is_none(), "empty from omitted: {json}");
        let back: Freeze = serde_json::from_value(json).expect("round-trip");
        assert_eq!(back, f);
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

    // ---- checked-in JSON Schema freshness (docs/schemas/) ----

    /// The JSON Schemas under `docs/schemas/` must match what
    /// `schema_for!` produces today — a Cargo.lock-style freshness guard
    /// so a `Schedule` / `Manifest` field change can't silently drift
    /// the operator-facing schema. The SPA editor, the backend
    /// `/api/schemas/*` endpoints, and these files all read the same
    /// derived shape; this test fails CI if the checked-in copy lags.
    /// Regenerate with:
    ///   `UPDATE_SCHEMAS=1 cargo test -p kanade-shared schema_files_are_current`
    #[test]
    fn schema_files_are_current() {
        assert_schema_file("schedule.schema.json", &schemars::schema_for!(Schedule));
        assert_schema_file("job.schema.json", &schemars::schema_for!(Manifest));
        assert_schema_file("view.schema.json", &schemars::schema_for!(View));
    }

    fn assert_schema_file(name: &str, schema: &schemars::Schema) {
        let generated = serde_json::to_string_pretty(schema).expect("serialize schema") + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/schemas")
            .join(name);
        if std::env::var_os("UPDATE_SCHEMAS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir docs/schemas");
            std::fs::write(&path, &generated).unwrap_or_else(|e| panic!("write {path:?}: {e}"));
            return;
        }
        // Normalize CRLF→LF before comparing: `.gitattributes` already
        // pins these files to `eol=lf`, but a stray CRLF working-tree
        // copy (autocrlf, a tool rewrite) shouldn't turn a *content*-
        // freshness check into a confusing line-ending failure — that's
        // .gitattributes' job, not this test's (gemini #588).
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| {
                panic!(
                    "read {path:?}: {e}\n\
                     generate it with: UPDATE_SCHEMAS=1 cargo test -p kanade-shared schema_files_are_current"
                )
            })
            .replace("\r\n", "\n");
        assert_eq!(
            on_disk, generated,
            "{name} is stale — a Schedule/Manifest schema change isn't reflected in docs/schemas/. \
             Refresh with: UPDATE_SCHEMAS=1 cargo test -p kanade-shared schema_files_are_current"
        );
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
    /// #418 operational constraints gating *when within an active
    /// period* a fire may happen: a maintenance `window`, a fleet
    /// `max_concurrent` cap, and `skip_dates` (holiday exclusion). The
    /// wall-clock ones are evaluated in the schedule's `tz`; future
    /// `require` (env gates) lands in the same namespace. Checked at
    /// tick time on both schedulers (and surfaced by `preview`).
    #[serde(default, skip_serializing_if = "Constraints::is_empty")]
    pub constraints: Constraints,
    /// #418 Phase 4: what to do after a fire's script comes back
    /// failed. Currently just `retry` (fixed-backoff in-process
    /// re-run); future `notify` / `disable` join the same namespace.
    /// Applied fire-side in `handle_command` (the retry policy is
    /// lowered onto every Command this schedule produces), so it
    /// covers both `runs_on` locations.
    #[serde(default, skip_serializing_if = "OnFailure::is_empty")]
    pub on_failure: OnFailure,
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
    /// Free-form operator taxonomy for the Schedules page — the
    /// schedule-side mirror of `Manifest.tags` (added in #640; a plain
    /// code ref rather than an intra-doc link, since that field isn't
    /// on this branch until #640 merges). Purely a SPA-side
    /// organisational aid (search / filter chips alongside the
    /// id-prefix grouping); the scheduler never reads it, so any
    /// string is allowed and it carries no firing semantics. A
    /// schedule's own tags are independent of its job's: the same job
    /// may back a `weekly` maintenance schedule and a `canary` rollout
    /// schedule. Empty by default and `skip_serializing_if`-elided per
    /// the #492 gradual-upgrade wire rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// GitOps provenance (#695) — see [`RepoOrigin`]. Stamped by
    /// `kanade schedule create` when the source YAML lives inside a Git
    /// work tree, so the SPA renders the schedule read-only and points
    /// edits back at the repo (SPEC design principle #3: 設定駆動 YAML +
    /// Git), parity with a job's [`Manifest::origin`]. `None` for
    /// SPA-born schedules and ones applied from outside any repo. Purely
    /// informational — the scheduler never reads it. New field ⇒ #492
    /// wire rule (`default` + `skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RepoOrigin>,
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
    /// #418 OS-native event trigger (`when: { on: [...] }`). There is
    /// no cron — the agent fires it from an OS event source (boot /
    /// session-change), not a tick — so the scheduler skips
    /// `tokio-cron` registration for it. Each event occurrence fires
    /// once, gated by the standard freeze / active / window /
    /// skip_dates checks.
    Event,
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
    /// #418 OS-native event trigger: `when: { on: [startup, logon] }`.
    /// Fires when the agent observes the listed OS event(s) rather than
    /// on a clock — there is no cron. `runs_on: agent` only (the agent
    /// owns the event source); [`Schedule::validate`] rejects it on
    /// `backend` and rejects an empty list. Each event occurrence fires
    /// once, gated by the same freeze / active / `constraints.window` /
    /// `skip_dates` checks as the cron path. `startup` fires once per OS
    /// boot (deduped via the host boot time); a `starting_deadline`, if
    /// set, limits it to "agent came up within that long after boot".
    On(Vec<OnTrigger>),
}

/// An OS event the agent can fire a schedule on (#418 `when: { on }`).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OnTrigger {
    /// Once per OS boot (the agent's first run for that boot). Catches
    /// freshly-imaged / reinstalled hosts at their next startup.
    Startup,
    /// On an interactive-session user logon — console, RDP, or
    /// auto-logon (Windows `WTS_SESSION_LOGON`). Does not fire for
    /// service / network / batch logons (no interactive session).
    Logon,
    /// When the workstation is locked (Win+L / idle lock; Windows
    /// `WTS_SESSION_LOCK`). Use for step-away compliance / cleanup.
    Lock,
    /// When the workstation is unlocked — the user returns to a locked
    /// session (Windows `WTS_SESSION_UNLOCK`). Use to re-check
    /// compliance / refresh state when work resumes.
    Unlock,
    /// When the host's network changes — IP address table change on
    /// connect / disconnect / DHCP renew / VPN / Wi-Fi roam (Windows
    /// `NotifyAddrChange`). Debounced agent-side (a burst of changes
    /// from one transition fires once after the network settles), so
    /// use it for "re-check connectivity / re-register on network move"
    /// rather than expecting one fire per raw adapter event.
    ///
    /// IPv4 only: `NotifyAddrChange` watches the IPv4 address table, so a
    /// transition that touches only IPv6 addresses won't fire. In practice
    /// dual-stack networks change both tables together, but a pure-IPv6
    /// move (e.g. an IPv6-only Wi-Fi roam) is not detected.
    NetworkChange,
}

/// Calendar time trigger (#418 Phase 2). `at` is either a time of
/// day (`"HH:MM"`, repeating — combine with `days`) or a full
/// date+time (`"YYYY-MM-DD HH:MM"`, a one-shot that fires once and
/// never again). Evaluated in the schedule's top-level `tz`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct CalendarSpec {
    /// `"HH:MM"` (24h) for a repeating trigger, or
    /// `"YYYY-MM-DD HH:MM"` (hyphen / slash / `T` separators all
    /// accepted) for a one-shot. Parsed lazily —
    /// [`Schedule::validate`] rejects garbage at create time.
    pub at: String,
    /// Day-of-week filter for a time-of-day `at`: `["mon-fri"]`,
    /// `["mon","wed","fri"]`, … (passed verbatim to the cron DOW
    /// field, so ranges and names both work). An **nth-weekday**
    /// `["tue#2"]` fires only on the 2nd Tuesday of each month
    /// ("Patch Tuesday"); the ordinal is `1..5`. A **last-weekday**
    /// `["friL"]` fires only on the last Friday of each month (handy
    /// for monthly maintenance). Empty = every day. Must be empty
    /// when `at` carries a date (the date already pins the day).
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
    /// `*`, a `-` range of those, an **nth-weekday** like `tue#2`
    /// (2nd Tuesday of the month — "Patch Tuesday"), or a
    /// **last-weekday** like `friL` (last Friday of the month).
    fn validate_days(&self) -> Result<(), String> {
        const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        let is_day = |p: &str| NAMES.contains(&p) || p.parse::<u8>().is_ok_and(|n| n <= 7);
        for tok in &self.days {
            // Report the whole token on a malformed range like `mon-`
            // (which would otherwise split to a cryptic empty part —
            // claude #432 follow-up).
            let invalid = |reason: &str| {
                Err(format!(
                    "when.days: invalid day token '{tok}' ({reason}; \
                     want mon..sun, 0-7, a range like mon-fri, an nth-weekday \
                     like tue#2, a last-weekday like friL, or *)"
                ))
            };
            // #418: nth-weekday suffix (`tue#2` = 2nd Tuesday). Croner
            // accepts `<dow>#<n>` (n = 1..5) in the DOW field, and
            // `to_cron` passes the token through verbatim, so the
            // engine fires only on that occurrence. It's a single
            // weekday + ordinal — not combinable with a range.
            if let Some((day_part, nth_part)) = tok.split_once('#') {
                // Normalize once and use `d` consistently (gemini #547);
                // the outer `invalid` already echoes the raw `tok`.
                let d = day_part.trim().to_ascii_lowercase();
                if d.contains('-') || !is_day(&d) {
                    return invalid("the part before # must be a single weekday");
                }
                match nth_part.trim().parse::<u8>() {
                    Ok(n) if (1..=5).contains(&n) => {}
                    _ => return invalid("the # ordinal must be 1..5 (e.g. tue#2 = 2nd Tuesday)"),
                }
                continue;
            }
            // #418: last-weekday suffix (`friL` = last Friday of the
            // month — the monthly-maintenance sibling of Patch Tuesday).
            // Croner accepts `<dow>L` in the DOW field with verified
            // last-<dow>-of-month semantics, and `to_cron` passes it
            // through verbatim. A single weekday + `L` — bare `L` and
            // ranges are rejected (croner would read bare `L` as
            // Saturday, which is a confusing footgun).
            if let Some(day_part) = tok.strip_suffix(['L', 'l']) {
                // No `.trim()`: a cron DOW token can't carry internal
                // whitespace, so `"fri L"` must be *rejected* here (its
                // strip leaves `"fri "`, and `is_day` catches the space)
                // rather than trimmed into a clean `"fri"` that then
                // produces a malformed `fri L` cron downstream and a
                // confusing croner error (gemini #560).
                let d = day_part.to_ascii_lowercase();
                if d.is_empty() {
                    return invalid("`L` (last-weekday) needs a weekday before it, e.g. friL");
                }
                if d.contains('-') || !is_day(&d) {
                    return invalid(
                        "the part before L must be a single weekday (e.g. friL = last Friday)",
                    );
                }
                continue;
            }
            for part in tok.split('-') {
                let p = part.trim().to_ascii_lowercase();
                if p.is_empty() {
                    return invalid("empty range bound");
                }
                if p != "*" && !is_day(&p) {
                    return invalid(&format!("'{part}' is not a day"));
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

    /// The wall-clock time-of-day this calendar fires at (`None` if
    /// `at` is unparseable — validate() guards that). Used to detect
    /// a calendar whose fire time can never fall inside its
    /// `constraints.window` (claude #452 review).
    pub fn fire_time(&self) -> Option<chrono::NaiveTime> {
        let p = self.parse_at().ok()?;
        chrono::NaiveTime::from_hms_opt(p.hour, p.minute, 0)
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

    /// The wall-clock time-of-day `now` reads as in this tz — used by
    /// [`Constraints::allows`] to test a maintenance window
    /// (#418 Phase 3). `Utc` is the naive UTC time; `Local` is the
    /// running host's local time.
    fn wall_time(self, now: chrono::DateTime<chrono::Utc>) -> chrono::NaiveTime {
        match self {
            ScheduleTz::Utc => now.time(),
            ScheduleTz::Local => now.with_timezone(&chrono::Local).time(),
        }
    }

    /// The wall-clock *date* `now` reads as in this tz — used by
    /// [`Constraints::allows`] to test `skip_dates` (#418 holiday
    /// exclusion). Same tz semantics as [`Self::wall_time`].
    fn wall_date(self, now: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDate {
        match self {
            ScheduleTz::Utc => now.date_naive(),
            ScheduleTz::Local => now.with_timezone(&chrono::Local).date_naive(),
        }
    }

    /// Stable lowercase wire/display label (`local` / `utc`) — matches
    /// the serde `snake_case` representation. Used for the preview
    /// response's `tz` field so the JSON shape isn't coupled to the
    /// `Debug` repr (claude #578 review).
    pub fn as_str(self) -> &'static str {
        match self {
            ScheduleTz::Local => "local",
            ScheduleTz::Utc => "utc",
        }
    }
}

impl std::fmt::Display for ScheduleTz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
/// struct variant). `{ evry: 6h }` still fails to parse (the
/// required `every` key is missing), and the create boundaries
/// reject the unknown `evry` via [`crate::strict`] with its path —
/// while agents reading a future writer's extra fields tolerate
/// them (#492).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
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
            When::On(triggers) => {
                let names: Vec<&str> = triggers.iter().map(|t| t.as_str()).collect();
                write!(f, "on [{}]", names.join(","))
            }
        }
    }
}

impl OnTrigger {
    /// Lowercase wire/display label (matches the serde `snake_case`).
    pub fn as_str(self) -> &'static str {
        match self {
            OnTrigger::Startup => "startup",
            OnTrigger::Logon => "logon",
            OnTrigger::Lock => "lock",
            OnTrigger::Unlock => "unlock",
            OnTrigger::NetworkChange => "network_change",
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

/// Host-environment gate (#418 `constraints.require`). Fire only when
/// the target host is in the required state. Sensed **in-process by the
/// agent** (Win32), so it is `runs_on: agent` only — the backend cannot
/// read a target host's power/idle state ([`Schedule::validate`]
/// rejects it on `runs_on: backend`, symmetric with `when: { on }`).
///
/// Evaluated at fire time as a skip-this-tick gate (NOT in
/// [`Constraints::allows`], which stays pure for `preview`): a reconcile
/// cadence re-checks every minute (so it effectively defers until the
/// state is met — the intended pairing); a `calendar` fire that lands
/// while the state is unmet is simply missed, same as `window`. It is
/// therefore a *runtime* gate and does not appear in `preview`.
// No `Eq`: `cpu_below: Option<f64>` is only `PartialEq` (f64 is not Eq).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default, PartialEq)]
pub struct Require {
    /// Fire only while on **AC power** (skip on battery). Reads
    /// `GetSystemPowerStatus`; an unknown/unreadable status is treated
    /// as not-on-AC (fail-closed — a restrictive gate must not fire
    /// when it can't confirm the condition). `false` (default) = no
    /// power requirement.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ac_power: bool,
    /// Fire only when the active console session has had **no keyboard /
    /// mouse input for at least this long** (humantime, e.g. `"10m"`) —
    /// "don't run while the user is actively working". Input-based
    /// (simpler than Task Scheduler's CPU/disk-aware idle). A
    /// headless / disconnected console (no interactive user) trivially
    /// satisfies it. `None` (default) = no idle requirement. Parsed
    /// lazily; [`Schedule::validate`] rejects garbage at create time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
    /// Fire only when the **whole-machine CPU usage is below this
    /// percent** (0–100; e.g. `20.0` = "system CPU < 20%") — "don't run
    /// while the box is busy". Reuses the agent's `host_perf` system CPU%
    /// sample (`sysinfo` mean over cores), so the reading is up to one
    /// `host_perf` cadence old (default 60s) — fine as a "generally
    /// busy?" proxy, and more accurate than a fresh one-shot read (CPU%
    /// needs two samples). An unavailable sample (host_perf not warmed
    /// up yet, or stale) is treated as "not below" (fail-closed — a
    /// restrictive gate must not fire when it can't confirm). `None`
    /// (default) = no CPU requirement. [`Schedule::validate`] rejects an
    /// out-of-range value at create time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_below: Option<f64>,
    /// Fire only when the host has **internet connectivity** (Windows
    /// `GetNetworkConnectivityHint` reports InternetAccess) — "don't run
    /// until online" for jobs that download / phone home. A captive
    /// portal (ConstrainedInternetAccess), LAN-only (LocalAccess), or
    /// unknown/unreadable state is treated as offline (fail-closed) — a
    /// portal would just fail a download, so we hold the run. For VPN /
    /// SASE / app-specific conditions, use a custom script gate (separate
    /// slice). `false` (default) = no network requirement.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub network: bool,
}

impl Require {
    /// `skip_serializing_if` helper for an embedded empty `require`.
    pub fn is_empty(&self) -> bool {
        !self.ac_power && self.idle.is_none() && self.cpu_below.is_none() && !self.network
    }

    /// Parsed minimum-idle duration (`None` = no idle requirement, or an
    /// unparseable value — `validate` rejects the latter at create time).
    pub fn min_idle(&self) -> Option<std::time::Duration> {
        self.idle
            .as_deref()
            .and_then(|s| humantime::parse_duration(s.trim()).ok())
    }

    /// First unparseable field for create-time rejection (mirrors
    /// [`Constraints::bad_skip_date`]).
    pub fn bad_idle(&self) -> Option<String> {
        self.idle.as_deref().and_then(|s| {
            humantime::parse_duration(s.trim())
                .err()
                .map(|e| format!("constraints.require.idle: invalid duration '{s}': {e}"))
        })
    }
}

/// Host-environment state sensed by the agent, fed to [`require_met`].
/// A named struct (not positional args) so the growing set of sensed
/// signals — several of them `bool` — can't be transposed at a call
/// site. The Win32 sensing lives in `kanade-agent::env_gate`.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvState {
    /// Is the host on AC power (`false` if on battery or unreadable).
    pub ac_online: bool,
    /// How long the console has been idle (`None` = couldn't determine).
    pub idle: Option<std::time::Duration>,
    /// Whole-machine CPU usage 0–100 (`None` = no sample yet).
    pub cpu_pct: Option<f64>,
    /// Does the host have internet connectivity (`false` if offline /
    /// LAN-only / unreadable).
    pub network_up: bool,
}

/// Pure env-gate decision (#418 `constraints.require`). The Win32
/// sensing lives in the agent (`kanade-agent::env_gate`); this is the
/// testable core, fed the already-sensed [`EnvState`]. Deliberately a
/// free fn (not folded into [`Constraints::allows`]) so `allows` stays
/// pure and `preview` never evaluates a runtime gate. Each set
/// requirement is a restrictive AND: any unmet (or unknown) gate skips.
pub fn require_met(req: &Require, env: &EnvState) -> bool {
    if req.ac_power && !env.ac_online {
        return false;
    }
    if let Some(min) = req.min_idle() {
        match env.idle {
            Some(d) if d >= min => {}
            _ => return false,
        }
    }
    if let Some(max) = req.cpu_below {
        match env.cpu_pct {
            Some(p) if p < max => {}
            _ => return false,
        }
    }
    if req.network && !env.network_up {
        return false;
    }
    true
}

/// [`Active`] decides *over what date range* a schedule is live,
/// `Constraints` decides *when, within an active period,* a fire is
/// allowed: `window` (a maintenance time-of-day window),
/// `max_concurrent` (a fleet-wide running-instance cap), `skip_dates`
/// (holiday exclusion) and `require` (host-environment gates, agent-only
/// — see [`Require`]).
// No `Eq`: contains `require: Option<Require>` which holds an f64.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default, PartialEq)]
pub struct Constraints {
    /// `"HH:MM-HH:MM"` wall-clock window (evaluated in the schedule's
    /// `tz`). Fires outside it are skipped — mainly for reconcile
    /// cadences ("patrol every 6h, but only fire overnight") and
    /// daytime change-freezes. `start > end` crosses midnight
    /// (`"22:00-05:00"` = 22:00 through 05:00 next morning). Parsed
    /// lazily; [`Schedule::validate`] rejects garbage at create time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Fleet-wide cap on how many instances of this schedule's job may
    /// run **at the same time** (#418 "同時実行ハード上限"). The
    /// backend scheduler counts the job's still-in-flight runs
    /// (`execution_results.finished_at IS NULL`) each tick and only
    /// dispatches to as many remaining pcs as there are free slots —
    /// a rolling window that refills as runs complete. Useful for
    /// disk/CPU/network-heavy jobs you don't want hammering the whole
    /// fleet at once.
    ///
    /// **Backend-only** (it needs a central counter): combining it
    /// with `runs_on: agent` is rejected by [`Schedule::validate`]
    /// (#418 decision E — "中央上限には中央が要る"). Most meaningful
    /// for `per_pc` reconcile cadences, where the poll re-ticks and
    /// refills slots. `None` (default) = no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// Calendar dates the schedule must **not** fire on — holidays,
    /// blackout days, one-off freeze dates (#418 "祝日除外"). Each is
    /// `YYYY-MM-DD`, evaluated as a wall-clock date in the schedule's
    /// `tz`. Applies to every `when` shape (a reconcile cadence skips
    /// the whole day; a calendar fire landing on the date is
    /// suppressed) and is honored by both the live scheduler and
    /// `preview`, since both gate on [`Constraints::allows`]. Empty
    /// (default) = no skips. Operator-supplied: there is no built-in
    /// holiday calendar — list the dates you care about. Parsed lazily;
    /// [`Schedule::validate`] rejects a malformed date at create time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_dates: Vec<String>,
    /// Host-environment gate (#418): fire only when the target host is
    /// in the required state (on AC power, idle). Agent-sensed at fire
    /// time, `runs_on: agent` only. See [`Require`]. `None` (default) =
    /// no environment requirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<Require>,
}

impl Constraints {
    /// `skip_serializing_if` helper — empty constraints are omitted
    /// from the wire format entirely.
    pub fn is_empty(&self) -> bool {
        self.window.is_none()
            && self.max_concurrent.is_none()
            && self.skip_dates.is_empty()
            && self.require.as_ref().is_none_or(Require::is_empty)
    }

    /// The first unparseable `skip_dates` entry, if any — the
    /// scheduler logs it at register time so a fail-closed
    /// (never-firing) schedule from a hand-edited KV blob is
    /// diagnosable, mirroring [`Schedule::bad_window`].
    pub fn bad_skip_date(&self) -> Option<String> {
        self.skip_dates.iter().find_map(|s| {
            chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                .err()
                .map(|e| format!("constraints.skip_dates: invalid date '{s}': {e}"))
        })
    }

    /// Parse `"HH:MM-HH:MM"` into `(start, end)`. Equal bounds are an
    /// error (a zero-width or all-day window is ambiguous — write no
    /// window for "always").
    pub fn parse_window(s: &str) -> Result<(chrono::NaiveTime, chrono::NaiveTime), String> {
        let (a, b) = s
            .split_once('-')
            .ok_or_else(|| format!("constraints.window: '{s}' must be 'HH:MM-HH:MM'"))?;
        let parse = |part: &str| {
            chrono::NaiveTime::parse_from_str(part.trim(), "%H:%M")
                .map_err(|e| format!("constraints.window: invalid time '{}': {e}", part.trim()))
        };
        let (start, end) = (parse(a)?, parse(b)?);
        if start == end {
            return Err(format!(
                "constraints.window: start and end are equal ('{s}'); omit window for 'always'"
            ));
        }
        Ok((start, end))
    }

    /// Is a fire allowed at `now` (evaluated in `tz`)? No window =
    /// always allowed. Half-open `[start, end)`; `start > end`
    /// crosses midnight.
    ///
    /// **Fail-closed** on an unparseable window (returns `false`,
    /// gemini #452 review): a window is a *restrictive* constraint
    /// (change-freeze / overnight-only), so a corrupt one must NOT
    /// silently allow fires during the restricted hours. Bad windows
    /// are rejected at create time by [`Schedule::validate`]; this
    /// only bites a hand-edited KV blob, where blocking is the safe
    /// direction. The scheduler warns at register time
    /// ([`Schedule::bad_window`]) so a stuck schedule is diagnosable.
    /// The tick path never panics regardless.
    pub fn allows(&self, now: chrono::DateTime<chrono::Utc>, tz: ScheduleTz) -> bool {
        // #418 holiday / blackout dates: never fire on a listed wall
        // date (in `tz`). Checked before the window since a skipped day
        // overrides any within-window allowance. Fail-closed on a
        // corrupt entry (same posture as `window`): a skip date is a
        // *restrictive* constraint, so a garbled one must not silently
        // re-enable fires — it blocks until fixed (`validate` rejects it
        // at create time; `bad_skip_date` lets the scheduler warn).
        if !self.skip_dates.is_empty() {
            let today = tz.wall_date(now);
            let blocked = self.skip_dates.iter().any(|s| {
                match chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
                    Ok(d) => d == today,
                    Err(_) => true, // corrupt entry → fail-closed (block)
                }
            });
            if blocked {
                return false;
            }
        }
        match self.window.as_deref() {
            // No window → always allowed.
            None => true,
            // Window set: membership, or fail-closed if unparseable
            // (`window_contains` returns None for a corrupt window).
            Some(_) => self.window_contains(tz.wall_time(now)).unwrap_or(false),
        }
    }

    /// Membership of a wall-clock time-of-day in the window. `None`
    /// when there is no window or it's unparseable (callers decide
    /// the failure direction). `start > end` crosses midnight.
    fn window_contains(&self, t: chrono::NaiveTime) -> Option<bool> {
        let (start, end) = Self::parse_window(self.window.as_deref()?).ok()?;
        Some(if start <= end {
            start <= t && t < end
        } else {
            t >= start || t < end
        })
    }
}

/// What to do when a fire's script fails (#418 Phase 4 — the "高"
/// retry/backoff gap). Where [`Constraints`] gates *whether* a fire
/// happens, `OnFailure` decides what happens *after* one ran and
/// came back bad. Only `retry` so far; future `notify` / `disable`
/// would join the same namespace.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
pub struct OnFailure {
    /// Re-run the script in-process when it exits non-zero (or times
    /// out), up to a cap, with a fixed backoff between attempts.
    /// `None` (default) = no retry: a failed run is published as-is
    /// and (for reconcile cadences) simply re-fires on the next poll
    /// tick. See [`Retry`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
}

impl OnFailure {
    /// `skip_serializing_if` helper — an empty policy is omitted from
    /// the wire format entirely.
    pub fn is_empty(&self) -> bool {
        self.retry.is_none()
    }

    /// Lower the operator-facing `retry` (humantime backoff) onto the
    /// engine vocabulary the agent's executor runs on (backoff in
    /// whole seconds). Single seam shared by the backend command
    /// builder and the agent's local scheduler so the two stamp the
    /// same [`crate::wire::RetrySpec`] onto every Command. Returns
    /// `None` when there is no retry policy or the backoff is
    /// unparseable (validate() rejects the latter at create time;
    /// this stays fail-safe = "no retry" for a hand-edited KV blob
    /// rather than panicking on the fire path).
    pub fn lowered_retry(&self) -> Option<crate::wire::RetrySpec> {
        let r = self.retry.as_ref()?;
        let backoff_secs = humantime::parse_duration(&r.backoff).ok()?.as_secs();
        Some(crate::wire::RetrySpec {
            max: r.max,
            backoff_secs,
        })
    }
}

/// Fixed-backoff retry policy (#418 Phase 4). `max` is the number of
/// *additional* attempts after the first run (so `max: 3` = up to 4
/// total executions); `backoff` is the humantime delay slept between
/// attempts. The retry happens fire-side (inside `kanade fire` /
/// `handle_command`) on every OS for the PoC — the Windows-native
/// "restart on failure" Task Scheduler path is deferred to the
/// native-delegation phase (#418 decision H).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct Retry {
    /// Max additional attempts after the first failure. Bounded
    /// `1..=10` by [`Schedule::validate`] — a typo'd `max: 1000`
    /// with a short backoff would otherwise pin a flapping script in
    /// a tight loop for the whole window.
    pub max: u32,
    /// Humantime delay slept between attempts (`"10m"`, `"30s"`).
    pub backoff: String,
}

/// Fleet-wide change-freeze (#418 Phase 5 — the "メンテナンス窓 /
/// 変更凍結" gap's global half). Where [`Constraints::window`] is a
/// *per-schedule* time-of-day gate, a `Freeze` is a *single, fleet-
/// global* "stop all automated change" switch the operator flips
/// during an incident or a year-end change-freeze. It lives in its
/// own KV singleton ([`crate::kv::KEY_FREEZE`]); when present and
/// active, both the backend scheduler and every agent's local
/// scheduler skip *every* fire.
///
/// Shapes:
/// * `{}` (no bounds) — frozen indefinitely until the operator
///   clears it (incident "big red button").
/// * `{ from, until }` — frozen only within `[from, until)`,
///   evaluated in `tz` (planned change-freeze; auto-thaws).
///
/// The KV key being *absent* means "not frozen" — so clearing the
/// freeze is a KV delete, and `is_active` only ever runs on a freeze
/// the operator actually set.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
pub struct Freeze {
    /// Frozen from this instant (RFC3339 or bare `YYYY-MM-DD` in
    /// `tz`). `None` ⇒ frozen from the beginning of time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Thawed from this instant on, exclusive. `None` ⇒ frozen with
    /// no scheduled end (manual clear required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Operator-supplied note surfaced on the freeze-skip log and the
    /// SPA banner ("year-end change freeze", "INC-1234"). Advisory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Timezone the bare-date bounds are evaluated in (RFC3339 bounds
    /// carry their own offset). Defaults to host-local like a
    /// schedule's `tz`.
    #[serde(default)]
    pub tz: ScheduleTz,
}

impl Freeze {
    /// Is the fleet frozen at `now`? An empty window (`from`/`until`
    /// both absent) is frozen unconditionally; otherwise membership of
    /// `[from, until)` in `tz`. Half-open like [`Active::contains`],
    /// but **fails CLOSED** on an unparseable bound — a freeze is a
    /// safety switch, so a corrupt window (only reachable via a
    /// hand-edited KV blob; `validate` rejects it at set time) must
    /// mean "frozen", not "fire normally" (coderabbit #472). This is
    /// the one deliberate divergence from `active`'s fail-OPEN
    /// behaviour, where an unparseable bound dormant-skips a schedule.
    pub fn is_active(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        // Parse a bound; an unparseable one short-circuits the whole
        // check to `true` (frozen) via the closure's `None` sentinel
        // handled below.
        let bound = |s: &Option<String>| -> Result<Option<chrono::DateTime<chrono::Utc>>, ()> {
            match s.as_deref() {
                None => Ok(None),
                Some(raw) => Active::parse_bound(raw, self.tz).map(Some).map_err(|_| ()),
            }
        };
        let (from, until) = match (bound(&self.from), bound(&self.until)) {
            (Ok(f), Ok(u)) => (f, u),
            // Any corrupt bound → fail closed (frozen).
            _ => return true,
        };
        if from.is_some_and(|f| now < f) {
            return false;
        }
        if until.is_some_and(|u| now >= u) {
            return false;
        }
        true
    }

    /// Reject unparseable bounds / `from >= until` at set time (the
    /// API + CLI counterpart to [`Schedule::validate`]).
    pub fn validate(&self) -> Result<(), String> {
        let from = self
            .from
            .as_deref()
            .map(|s| Active::parse_bound(s, self.tz))
            .transpose()
            .map_err(|e| e.replace("active:", "freeze:"))?;
        let until = self
            .until
            .as_deref()
            .map(|s| Active::parse_bound(s, self.tz))
            .transpose()
            .map_err(|e| e.replace("active:", "freeze:"))?;
        if let (Some(f), Some(u)) = (from, until) {
            if f >= u {
                return Err(format!(
                    "freeze.from ({}) must be strictly before freeze.until ({})",
                    self.from.as_deref().unwrap_or_default(),
                    self.until.as_deref().unwrap_or_default(),
                ));
            }
        }
        Ok(())
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
    /// The error message if this schedule's `constraints.window` is
    /// set but unparseable, else `None`. The scheduler logs this at
    /// register time so a fail-closed (never-firing) schedule from a
    /// hand-edited KV blob is diagnosable (gemini #452 review).
    pub fn bad_window(&self) -> Option<String> {
        let w = self.constraints.window.as_deref()?;
        Constraints::parse_window(w).err()
    }

    /// True when this is a `calendar` schedule whose fire time can
    /// never fall inside its `constraints.window` — the cron fires,
    /// the window check rejects it, and (firing only at that
    /// time-of-day) it effectively never runs. An easy misconfig to
    /// set up by accident; the scheduler warns at register time
    /// (claude #452 review). Reconcile shapes poll every minute, so
    /// they always catch the window opening and aren't affected.
    pub fn calendar_outside_window(&self) -> bool {
        let When::Calendar(c) = &self.when else {
            return false;
        };
        let Some(t) = c.fire_time() else {
            return false;
        };
        matches!(self.constraints.window_contains(t), Some(false))
    }

    /// Up to `count` future instants this schedule will fire, as
    /// absolute UTC, strictly after `now` — the dry-run / preview
    /// surface (#418 "ドライラン / プレビュー"). Only **calendar**
    /// schedules have discrete fire times; reconcile shapes
    /// (`per_pc`/`per_target`) poll every minute gated by cooldown, so
    /// they return an empty vec and the caller describes the cadence
    /// instead. Occurrences outside the `active.{from,until}` window or
    /// the `constraints.window` are **skipped**, so the list reflects
    /// when the schedule will ACTUALLY run, not the raw cron ticks.
    /// Evaluated in the schedule's `tz`, exactly like the scheduler's
    /// `Job::new_async_tz`, and with the same croner config the
    /// scheduler / [`Schedule::validate`] use, so a preview can never
    /// disagree with a real fire. A schedule that can never fire (a
    /// calendar time wholly outside its window, a past one-shot,
    /// `enabled: false` is *not* considered here — callers gate on
    /// `enabled` separately) yields an empty vec.
    pub fn preview_fires(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        count: usize,
    ) -> Vec<chrono::DateTime<chrono::Utc>> {
        use croner::parser::{CronParser, Seconds};
        if !matches!(self.when, When::Calendar(_)) {
            return Vec::new();
        }
        // Same lowering + croner config as `next_calendar_fire` and the
        // live scheduler, so a preview can never disagree with a real
        // fire. `preview_fires` adds the N-occurrence walk and the
        // active / window filtering on top of that single seam.
        let lowered = self.lowered();
        let Ok(cron) = CronParser::builder()
            .seconds(Seconds::Required)
            .dom_and_dow(true)
            .build()
            .parse(&lowered.cron)
        else {
            return Vec::new();
        };
        let accept = |utc: chrono::DateTime<chrono::Utc>| {
            self.active.contains(utc, self.tz) && self.constraints.allows(utc, self.tz)
        };
        match self.tz {
            ScheduleTz::Utc => Self::next_occurrences(&cron, now, count, accept),
            ScheduleTz::Local => {
                Self::next_occurrences(&cron, now.with_timezone(&chrono::Local), count, accept)
            }
        }
    }

    /// Walk croner forward from `after` collecting up to `count`
    /// accepted occurrences (converted to UTC). Generic over the tz the
    /// cron is evaluated in so `preview_fires` can run it in either
    /// `Utc` or `Local` without duplicating the loop.
    fn next_occurrences<Tz>(
        cron: &croner::Cron,
        after: chrono::DateTime<Tz>,
        count: usize,
        accept: impl Fn(chrono::DateTime<chrono::Utc>) -> bool,
    ) -> Vec<chrono::DateTime<chrono::Utc>>
    where
        Tz: chrono::TimeZone,
    {
        // Bound the scan so an `active`/window dead-end (every future
        // tick rejected) can't spin forever: ~4096 raw ticks covers
        // >10y of a daily calendar while staying instant for croner.
        const SCAN_CAP: usize = 4096;
        let mut out = Vec::with_capacity(count.min(SCAN_CAP));
        let mut cursor = after;
        let mut scanned = 0usize;
        while out.len() < count && scanned < SCAN_CAP {
            scanned += 1;
            let Ok(next) = cron.find_next_occurrence(&cursor, false) else {
                break;
            };
            let utc = next.with_timezone(&chrono::Utc);
            if accept(utc) {
                out.push(utc);
            }
            // `find_next_occurrence(.., inclusive = false)` already
            // advances strictly past `cursor`, so handing it `next`
            // verbatim gets the following occurrence — no manual +1s
            // nudge (and `DateTime<Tz>` is `Copy`, so no clone).
            cursor = next;
        }
        out
    }

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
            // Event triggers have no cron — the agent fires them from an
            // OS event source. The `# event-trigger` cron is never
            // registered (the scheduler branches on `is_event()` first),
            // but keep it deliberately-invalid as a belt-and-suspenders
            // so a stray registration would fail rather than misfire.
            When::On(_) => Lowered {
                cron: "# event-trigger (no cron)".into(),
                mode: ExecMode::Event,
                cooldown: None,
                tz,
            },
        }
    }

    /// True when this schedule fires from an OS event (`when: { on }`)
    /// rather than a clock — the agent skips `tokio-cron` registration
    /// for these and drives them from boot / session-change instead.
    pub fn is_event(&self) -> bool {
        matches!(self.when, When::On(_))
    }

    /// The OS event triggers this schedule listens for, or `&[]` when it
    /// is not an event schedule.
    pub fn event_triggers(&self) -> &[OnTrigger] {
        match &self.when {
            When::On(t) => t,
            _ => &[],
        }
    }

    /// The next absolute (UTC) time this schedule fires, or `None` when
    /// it has no discrete upcoming fire to preview.
    ///
    /// Used by the KLP `maintenance.list` preview ("what's about to
    /// happen on my PC", SPEC §2.1). Returns `None` for:
    ///
    /// - reconcile shapes (`per_pc` / `per_target`) — they lower to the
    ///   every-minute [`POLL_CRON`] and re-converge state continuously,
    ///   so "next fire" is always ~60s away and means nothing to a user
    ///   previewing upcoming maintenance;
    /// - a calendar schedule whose lowered cron won't parse (a
    ///   hand-edited KV blob that slipped past [`Schedule::validate`]);
    /// - a cron with no future occurrence.
    ///
    /// The wall-clock fire is evaluated in the schedule's own `tz`
    /// (matching the live tick's `Job::new_async_tz`) then normalised
    /// to UTC for the wire. `inclusive = false`: strictly the *next*
    /// fire after `now`, never one matching the current instant.
    pub fn next_calendar_fire(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        if !matches!(self.when, When::Calendar(_)) {
            return None;
        }
        let lowered = self.lowered();
        // Same parser configuration tokio-cron-scheduler 0.15 uses
        // internally, so this can never compute a fire the live
        // scheduler wouldn't (seconds required, DOM-and-DOW honored).
        let cron = croner::parser::CronParser::builder()
            .seconds(croner::parser::Seconds::Required)
            .dom_and_dow(true)
            .build()
            .parse(&lowered.cron)
            .ok()?;
        match lowered.tz {
            ScheduleTz::Utc => cron.find_next_occurrence(&now, false).ok(),
            ScheduleTz::Local => {
                let now_local = now.with_timezone(&chrono::Local);
                cron.find_next_occurrence(&now_local, false)
                    .ok()
                    .map(|t| t.with_timezone(&chrono::Utc))
            }
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
        // #418 event triggers: the agent owns the OS event source
        // (boot / session-change), so `when: { on }` is agent-only and
        // needs at least one trigger.
        if let When::On(triggers) = &self.when {
            if !matches!(self.runs_on, RunsOn::Agent) {
                return Err(
                    "when.on (OS event trigger) is fired by the agent's own event \
                     source, so it requires runs_on: agent"
                        .into(),
                );
            }
            if triggers.is_empty() {
                return Err(
                    "when.on must list at least one trigger (e.g. [startup, logon])".into(),
                );
            }
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
        // #418 Phase 3: a bad maintenance window is rejected at create
        // time (parse_window also catches equal bounds).
        if let Some(w) = self.constraints.window.as_deref() {
            Constraints::parse_window(w)?;
        }
        // #418 holiday exclusion: reject a malformed skip date at create
        // time so the fail-closed `allows` path only ever bites a
        // hand-edited KV blob, not a fresh `kanade schedule create`.
        if let Some(err) = self.constraints.bad_skip_date() {
            return Err(err);
        }
        // #418: constraints.max_concurrent is a central running-instance
        // cap, so it needs the backend's counter — reject it on
        // runs_on: agent (decision E), and reject a meaningless 0.
        if let Some(mc) = self.constraints.max_concurrent {
            // Check the structural incompatibility (agent has no central
            // counter) before the value range, so a `max_concurrent: 0`
            // + `runs_on: agent` combo reports the more fundamental
            // problem first (claude #542).
            if matches!(self.runs_on, RunsOn::Agent) {
                return Err(
                    "constraints.max_concurrent needs a central counter and is backend-only; \
                     it cannot be combined with runs_on: agent (each agent self-schedules, \
                     so there is no fleet-wide count to cap against)"
                        .into(),
                );
            }
            if mc == 0 {
                return Err(
                    "constraints.max_concurrent must be >= 1 (0 would never fire; \
                     omit it for no cap)"
                        .into(),
                );
            }
        }
        // #418: constraints.require (host-state env gates: ac_power /
        // idle / cpu_below / network) is sensed in-process by the agent,
        // so it needs runs_on: agent — the backend can't read a target
        // host's power / idle / cpu / connectivity state. Symmetric with
        // `when: { on }` (also agent-only); inverse of max_concurrent
        // (backend-only).
        if let Some(req) = &self.constraints.require {
            if !req.is_empty() && matches!(self.runs_on, RunsOn::Backend) {
                return Err(
                    "constraints.require (host-state env gates: ac_power / idle / cpu_below / \
                     network) is sensed in-process by the agent and needs runs_on: agent; the \
                     backend cannot read a target host's power / idle / cpu / connectivity state"
                        .into(),
                );
            }
            // Reject a malformed idle duration at create time so the
            // fail-closed runtime path only ever bites a hand-edited
            // KV blob (mirror skip_dates / on_failure.retry).
            if let Some(err) = req.bad_idle() {
                return Err(err);
            }
            // cpu_below is a percent — reject out-of-range so a typo
            // can't make a schedule that never (>=100 is always-busy?
            // no — <0 never matches) or trivially fires.
            if let Some(c) = req.cpu_below
                && !(c > 0.0 && c <= 100.0)
            {
                return Err(format!(
                    "constraints.require.cpu_below must be in (0, 100] percent (got {c}); \
                     omit it for no CPU requirement"
                ));
            }
        }
        // #418 Phase 4: a bad on_failure.retry is rejected at create
        // time — backoff must be valid humantime, and max is bounded
        // so a typo can't pin a flapping script in a tight loop.
        if let Some(r) = &self.on_failure.retry {
            let backoff = humantime::parse_duration(&r.backoff).map_err(|e| {
                format!(
                    "on_failure.retry.backoff: invalid duration '{}': {e}",
                    r.backoff
                )
            })?;
            // The wire form lowers backoff to whole seconds, so a
            // sub-second value would silently become a 0s no-wait
            // (coderabbit #466). Reject it rather than honour a backoff
            // the operator can't actually get.
            if backoff.as_secs() < 1 {
                return Err(format!(
                    "on_failure.retry.backoff must be >= 1s (got '{}'); sub-second backoffs \
                     round to 0 on the wire",
                    r.backoff
                ));
            }
            if !(1..=10).contains(&r.max) {
                return Err(format!(
                    "on_failure.retry.max must be 1..=10 (got {}); it counts additional \
                     attempts after the first run",
                    r.max
                ));
            }
        }
        // A blank / whitespace-only tag renders an empty filter chip on
        // the Schedules page — reject it at create time, mirroring the
        // Manifest::validate tag guard.
        for tag in &self.tags {
            if tag.trim().is_empty() {
                return Err("tags must not contain empty entries".to_string());
            }
        }
        Ok(())
    }
}

/// Shared `serde(default)` for `bool` fields that default to `true`
/// (e.g. `CheckHint::fleet` / `CheckHint::health`). Generic name so it
/// doesn't read as "fleet" when reused for `health`.
fn default_true() -> bool {
    true
}
