//! `jobs.*` method types — user-invokable job catalog + execute +
//! progress + kill.
//!
//! The Client App's three job-driven tabs (SPEC §2.1):
//! - "アップデート" lists `category: software_update` manifests
//! - "困ったとき" lists `category: troubleshoot` manifests
//! - Software catalog lists `category: catalog` manifests
//!
//! All three flow through the same `jobs.list` / `jobs.execute` /
//! `jobs.progress` pipeline — only the filter differs. Manifests
//! with `user_invokable: false` are invisible from KLP (the agent
//! filters before answering `jobs.list`, and rejects
//! `jobs.execute` with `Unauthorized` if a client tries to call
//! one directly).

use serde::{Deserialize, Serialize};

// ---------- shared types ----------

/// Job category from the manifest's `category:` field. Drives which
/// Client App tab the job appears in. `#[non_exhaustive]` leaves
/// room for SPEC additions (new tabs) without a wire bump.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobCategory {
    /// Chrome / Edge / Office / runtime updaters. Appears in the
    /// "アップデート" tab.
    SoftwareUpdate,
    /// Teams cache clear, Office repair, network reset, … Appears
    /// in the "困ったとき" tab.
    Troubleshoot,
    /// Self-service install catalog. Appears in the software
    /// catalog tab.
    Catalog,
    /// #492: serde-level forward-compat catch-all. `#[non_exhaustive]`
    /// only affects Rust match exhaustiveness — serde still hard-fails
    /// on an unknown variant STRING, so a newer peer's new variant
    /// used to make older readers reject the whole containing message.
    /// Unknown decodes any unrecognised value; UIs render it neutrally.
    #[serde(other)]
    Unknown,
}

/// Run-state machine for one `jobs.execute` invocation.
///
/// State transitions:
/// `Queued` → `Running` → `Completed` | `Failed` | `Killed`.
/// `Queued` ⇒ accepted but not started yet (waiting on the
/// concurrent-run cap or staleness check); the very first
/// `jobs.progress` push usually moves straight to `Running`.
/// `#[non_exhaustive]` so a future SPEC can add states like
/// `Skipped` (staleness gate) without a wire bump.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunStatus {
    /// Accepted, not yet spawned.
    Queued,
    /// `tokio::process::Command::spawn()` returned, script is
    /// running.
    Running,
    /// Exited with code 0 (or whatever the manifest declares as
    /// success).
    Completed,
    /// Exited non-zero, or a Layer 2 skipped-result was published.
    Failed,
    /// User-initiated kill via `jobs.kill`. Distinct from `Failed`
    /// so the SPA can show "stopped by you" instead of "errored".
    Killed,
    /// #492: serde-level forward-compat catch-all. `#[non_exhaustive]`
    /// only affects Rust match exhaustiveness — serde still hard-fails
    /// on an unknown variant STRING, so a newer peer's new variant
    /// used to make older readers reject the whole containing message.
    /// Unknown decodes any unrecognised value; UIs render it neutrally.
    #[serde(other)]
    Unknown,
}

/// One entry in `jobs.list` — the SPEC §2.12.11 reference shape,
/// extended with the `description` field used by the manifest's
/// existing `display_description`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct UserInvokableJob {
    /// Manifest id (matches everywhere else — `Command.id`,
    /// `ExecResult.manifest_id`).
    pub id: String,
    /// `display_name` from the manifest.
    pub display_name: String,
    /// `display_description` from the manifest. Renders as the row's
    /// subtitle in the Client App.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_description: Option<String>,
    /// Optional icon hint (lucide-react name or a `data:` URL).
    /// `None` means the SPA falls back to the category's default
    /// icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub category: JobCategory,
    /// Pinned version string from the manifest. Same field as
    /// `Manifest.version`.
    pub version: String,
    /// Snapshot of the last KLP-driven run of this job FOR THIS
    /// USER. `None` until they've executed it at least once.
    /// Backend keeps the cross-user / cross-PC history separately
    /// (operator-only `executions` table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<JobRun>,
}

/// Compact summary of a past run — what the Client App shows next
/// to the job's "Run again" button.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobRun {
    pub run_id: String,
    pub status: RunStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

// ---------- jobs.list ----------

/// `jobs.list` params — optional category filter (when the Client
/// App is showing a single tab and doesn't want the full set).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct JobsListParams {
    /// `None` ⇒ return every user-invokable job. `Some(c)` ⇒ filter
    /// to that category. The agent always strips
    /// `user_invokable: false` manifests regardless of filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<JobCategory>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobsListResult {
    pub items: Vec<UserInvokableJob>,
}

// ---------- jobs.execute ----------

/// `jobs.execute` params — the manifest id to run. Agent looks up
/// the manifest from KV at fire time, so a change to
/// `user_invokable` takes effect on the next execute attempt (SPEC
/// §2.1: "Agent 側で manifest を必ず再 lookup し、`user_invokable:
/// false` への変更が即時反映される").
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobsExecuteParams {
    /// Manifest id from `jobs.list[].id`.
    pub id: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobsExecuteResult {
    /// Agent-minted UUID for this specific run. Carried back to the
    /// caller so they can correlate the `jobs.progress` pushes that
    /// follow + later `jobs.kill` calls.
    pub run_id: String,
}

// ---------- jobs.subscribe ----------

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct JobsSubscribeParams {}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobsSubscribeResult {
    pub subscription: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobsUnsubscribeParams {
    pub subscription: String,
}

// ---------- jobs.progress (push) ----------

/// Push payload for `jobs.progress`. Sent on:
/// - first move from Queued → Running
/// - each stdout / stderr chunk (split to fit the 1 MiB framing
///   cap — SPEC §2.12.2)
/// - terminal state transition (Completed / Failed / Killed) with
///   `exit_code` populated
///
/// The reference shape is SPEC §2.12.11's `JobProgress` struct.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobProgress {
    /// The `run_id` minted by `jobs.execute`.
    pub run_id: String,
    pub status: RunStatus,
    /// Newly-produced stdout, UTF-8 decoded (tolerant — see
    /// `kanade-agent::process::capture_tolerant`). `None` when this
    /// push is a pure status transition; `Some("")` would never be
    /// emitted (the agent omits the field instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_chunk: Option<String>,
    /// Newly-produced stderr. Same conventions as `stdout_chunk`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_chunk: Option<String>,
    /// Populated on the terminal push only. Agents stamp the actual
    /// process exit code from the child. Synthetic non-process
    /// outcomes (timeout, remote kill) are surfaced as `Some(-1)`
    /// with the `status` field carrying the distinguishing
    /// information (`Failed` / `Killed`), not via a reserved
    /// exit-code number.
    ///
    /// Note: the sibling `ExecResult` wire (manifest exec →
    /// backend, NOT this KLP flow) DOES partition synthetic skip
    /// codes (124 / 125 / 126 / 127) for the agent's pre-exec
    /// staleness gates. See the doc on
    /// `kanade-agent::commands::publish_staleness_skipped` for
    /// the table. JobProgress's exit_code does not share that
    /// partition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

// ---------- jobs.kill ----------

/// `jobs.kill` params — `run_id` from this connection's earlier
/// `jobs.execute` call. SPEC §2.12.4 forbids cross-connection kill
/// (agent returns `Unauthorized`); a user wanting to stop another
/// user's job goes through the operator SPA, not the Client App.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobsKillParams {
    pub run_id: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct JobsKillResult {
    /// Wall-clock the agent dispatched the kill signal. The
    /// terminal `jobs.progress` push (status = `Killed`) follows
    /// asynchronously once the child process actually exits.
    pub requested_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn job_category_serialises_snake_case() {
        for (variant, expected) in [
            (JobCategory::SoftwareUpdate, "\"software_update\""),
            (JobCategory::Troubleshoot, "\"troubleshoot\""),
            (JobCategory::Catalog, "\"catalog\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "encode {variant:?}");
            let back: JobCategory = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant, "round-trip {expected}");
        }
    }

    #[test]
    fn run_status_serialises_snake_case() {
        for (variant, expected) in [
            (RunStatus::Queued, "\"queued\""),
            (RunStatus::Running, "\"running\""),
            (RunStatus::Completed, "\"completed\""),
            (RunStatus::Failed, "\"failed\""),
            (RunStatus::Killed, "\"killed\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "encode {variant:?}");
            let back: RunStatus = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant, "round-trip {expected}");
        }
    }

    #[test]
    fn user_invokable_job_minimum_shape_decodes() {
        // Backend that hasn't fully populated `display_description`
        // / `icon` / `last_run` must still produce decodable rows.
        let wire = r#"{
            "id":"chrome-update","display_name":"Chrome を更新",
            "category":"software_update","version":"1.2.0"
        }"#;
        let j: UserInvokableJob = serde_json::from_str(wire).unwrap();
        assert_eq!(j.id, "chrome-update");
        assert!(j.display_description.is_none());
        assert!(j.icon.is_none());
        assert!(j.last_run.is_none());
    }

    #[test]
    fn job_progress_status_transition_omits_chunks() {
        // Status-only push (Queued → Running) has neither stdout
        // nor stderr; both fields must be absent from the wire, not
        // null. Strict JS clients reject `null` strings.
        let p = JobProgress {
            run_id: "run-1".into(),
            status: RunStatus::Running,
            stdout_chunk: None,
            stderr_chunk: None,
            exit_code: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("stdout_chunk").is_none(), "wire: {v:?}");
        assert!(v.get("stderr_chunk").is_none(), "wire: {v:?}");
        assert!(v.get("exit_code").is_none(), "wire: {v:?}");
    }

    #[test]
    fn job_progress_terminal_push_carries_exit_code() {
        let p = JobProgress {
            run_id: "run-1".into(),
            status: RunStatus::Completed,
            stdout_chunk: None,
            stderr_chunk: None,
            exit_code: Some(0),
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["status"], "completed");
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn jobs_list_filter_optional() {
        // No filter ⇒ all categories. Wire form has no `category`
        // key, not `category: null`.
        let p = JobsListParams::default();
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("category").is_none(), "wire: {v:?}");
    }

    #[test]
    fn jobs_execute_result_round_trips() {
        let r = JobsExecuteResult {
            run_id: "run-uuid-1".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: JobsExecuteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id, "run-uuid-1");
    }

    #[test]
    fn job_run_serialises_with_optional_finish() {
        // In-flight run: started_at present, finished_at + exit_code
        // absent. Critical because the Client App's "last run" chip
        // uses `finished_at.is_some()` as the "row is terminal" flag.
        let r = JobRun {
            run_id: "run-1".into(),
            status: RunStatus::Running,
            started_at: chrono::Utc.with_ymd_and_hms(2026, 5, 24, 0, 0, 0).unwrap(),
            finished_at: None,
            exit_code: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("finished_at").is_none(), "wire: {v:?}");
        assert!(v.get("exit_code").is_none(), "wire: {v:?}");
    }

    #[test]
    fn unknown_enum_variants_decode_to_unknown() {
        // #492: a newer peer's new variant must not make this build
        // fail to decode the whole containing message — serde(other)
        // catches it (non_exhaustive alone never protected the wire).
        let s: RunStatus = serde_json::from_str("\"skipped\"").unwrap();
        assert_eq!(s, RunStatus::Unknown);
        let c: JobCategory = serde_json::from_str("\"future_tab\"").unwrap();
        assert_eq!(c, JobCategory::Unknown);
        // Known variants are untouched.
        let r: RunStatus = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(r, RunStatus::Running);
    }

    #[test]
    fn unknown_variant_round_trips() {
        // PR #558 review (gemini): Unknown must SERIALIZE cleanly too
        // — a node that decoded a newer peer's variant and re-emits
        // the containing message (e.g. an agent forwarding a
        // jobs.list entry) must not hit a runtime serialization
        // error. It serialises as "unknown" and decodes back to
        // Unknown on every #492-aware peer.
        let s = serde_json::to_string(&RunStatus::Unknown).unwrap();
        assert_eq!(s, "\"unknown\"");
        let back: RunStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back, RunStatus::Unknown);
    }

    #[test]
    fn unknown_decodes_across_the_other_wire_enums() {
        // PR #558 review (claude): cover the remaining #492 enums.
        use crate::ipc::error::ErrorKind;
        use crate::ipc::maintenance::DeferDuration;
        use crate::ipc::notifications::NotificationPriority;

        let k: ErrorKind = serde_json::from_str("\"FutureErrorKind\"").unwrap();
        assert_eq!(k, ErrorKind::Unknown);
        assert_eq!(k.code(), -32099);
        let known: ErrorKind = serde_json::from_str("\"Unauthorized\"").unwrap();
        assert_eq!(known, ErrorKind::Unauthorized);

        let p: NotificationPriority = serde_json::from_str("\"critical\"").unwrap();
        assert_eq!(p, NotificationPriority::Unknown);

        let d: DeferDuration = serde_json::from_str("\"2h\"").unwrap();
        assert_eq!(d, DeferDuration::Unknown);
        assert_eq!(d.as_duration(), chrono::Duration::minutes(15));
    }
}
