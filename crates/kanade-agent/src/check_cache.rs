//! Agent-side cache of operator-defined health-check results (#290).
//!
//! A job whose Manifest carries a `check:` hint prints a JSON object
//! on stdout; after a successful run the command path calls
//! [`CheckSink::record`], which maps the hint's `status_field` /
//! `detail_field` into a KLP [`Check`] and stores it keyed by check
//! name. The KLP state evaluator ([`crate::klp::state::eval_once`])
//! merges these cached checks into every `StateSnapshot.checks`, and
//! [`CheckSink::wait`] lets the evaluator re-publish immediately when
//! a new result lands instead of waiting for the 30 s tick.
//!
//! Cross-platform on purpose: the writer ([`CheckSink::record`] in
//! `commands.rs`) compiles everywhere, while the reader (`klp::state`)
//! is Windows-only. On non-Windows the cache is written but never
//! read — harmless.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kanade_shared::ipc::state::{Check, CheckStatus};
use kanade_shared::manifest::CheckHint;
use tokio::sync::Notify;

/// Shared, cheap-to-clone handle threaded into both the command
/// result path (writer) and the KLP state evaluator (reader).
#[derive(Clone)]
pub struct CheckSink {
    inner: Arc<Inner>,
}

struct Inner {
    results: Mutex<HashMap<String, Check>>,
    updated: Notify,
    /// Where the cache is persisted as JSON (`None` = in-memory only,
    /// used by tests / non-Windows). Operator-defined check results
    /// are written here on every `record` and re-loaded on boot so the
    /// Client App's Health tab survives an agent restart and shows the
    /// last-known status even while offline (#290). It's a bounded
    /// snapshot — one entry per check name, a few KB — so a single
    /// atomically-replaced JSON file is the right store (no SQLite;
    /// same pattern as `local_completions.json` / the outboxes).
    path: Option<PathBuf>,
    /// Check slugs whose defining job currently exists in `BUCKET_JOBS`,
    /// kept fresh by the local scheduler (see
    /// [`CheckSink::set_active_slugs`]). `checks()` filters its cached
    /// results to this set so a check whose job an operator **deleted**
    /// stops appearing on the client's Health tab — deleting a `check:`
    /// job otherwise leaves its last result stranded in the cache
    /// forever (no new result ever overwrites it).
    ///
    /// `None` (the initial value, before the scheduler's first resync)
    /// means "active set unknown — don't filter": a broker-down boot
    /// can't enumerate the live jobs, so we show the last-known checks
    /// rather than blanking the tab. Self-heals to `Some(..)` on the
    /// first successful resync.
    ///
    /// Wrapped in `Arc` so `checks()` (called every evaluator tick) reads
    /// it with a refcount bump instead of cloning the whole set on the
    /// read path.
    active_slugs: Mutex<Option<Arc<HashSet<String>>>>,
}

impl CheckSink {
    /// In-memory only (no persistence). For tests and the non-Windows
    /// build where the KLP reader isn't compiled.
    pub fn new() -> Self {
        Self::with_inner(HashMap::new(), None)
    }

    /// Load the cache from `path` (an empty cache if the file is
    /// missing or unreadable — a corrupt/old file must not stop the
    /// agent booting) and persist every subsequent `record` back to it.
    pub fn load(path: PathBuf) -> Self {
        let results = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<HashMap<String, Check>>(&bytes)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, path = %path.display(), "check_cache: ignoring unreadable persisted cache");
                    HashMap::new()
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "check_cache: failed to read persisted cache");
                HashMap::new()
            }
        };
        Self::with_inner(results, Some(path))
    }

    fn with_inner(results: HashMap<String, Check>, path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                results: Mutex::new(results),
                updated: Notify::new(),
                path,
                // Unknown until the scheduler's first resync — see the
                // field doc. Filtering stays off (show everything) until
                // then so a boot before the jobs KV is reachable doesn't
                // wipe the Health tab.
                active_slugs: Mutex::new(None),
            }),
        }
    }

    /// Store a freshly-built [`Check`] under its `name`, persist the
    /// cache, and wake the state evaluator. The caller builds the
    /// `Check` — [`build_check`] on a clean exit, [`build_check_failed`]
    /// when the script crashed — so this stays a dumb sink.
    pub fn record(&self, check: Check) {
        self.guarded(|map| {
            map.insert(check.name.clone(), check);
            // Persist UNDER the lock: two concurrent records can't
            // interleave and write an older snapshot over a newer one
            // (the previous out-of-lock version had that race). The
            // file is a few KB — sub-ms write — so holding the lock
            // briefly doesn't meaningfully delay a reader's `checks()`.
            self.persist(map);
        });
        // Re-publish the snapshot now instead of on the next 30 s tick.
        self.inner.updated.notify_one();
    }

    /// Atomically write the cache to its JSON file (temp + rename), if
    /// persistence is configured. Best-effort: a write failure is
    /// logged, not fatal — the in-memory cache is still authoritative
    /// for this run. Call while holding the results lock so writes are
    /// serialised.
    fn persist(&self, results: &HashMap<String, Check>) {
        let Some(path) = &self.inner.path else { return };
        let json = match serde_json::to_vec_pretty(results) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "check_cache: serialize failed");
                return;
            }
        };
        // The data dir may not exist yet on a fresh install.
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(error = %e, path = %parent.display(), "check_cache: create data dir failed");
                return;
            }
        }
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            tracing::warn!(error = %e, path = %tmp.display(), "check_cache: temp write failed");
            return;
        }
        // rename replaces the destination atomically (MoveFileEx with
        // REPLACE_EXISTING on Windows, rename(2) on Unix).
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::warn!(error = %e, path = %path.display(), "check_cache: atomic rename failed");
        }
    }

    /// Replace the set of check slugs whose defining job currently lives
    /// in `BUCKET_JOBS`. Called by the local scheduler whenever its jobs
    /// snapshot changes (resync + per-key watch events) so a **deleted**
    /// `check:` job's stale result drops off the Health tab instead of
    /// lingering forever (nothing ever overwrites a cached check once
    /// its job stops running). Wakes the evaluator so the prune lands on
    /// the next push rather than waiting up to the 30 s tick.
    ///
    /// Note this filters at read time only — `record`/`persist` still
    /// keep every result on disk, so a transient empty/stale active set
    /// (or a job that comes back) can never destroy the last-known data;
    /// the row simply reappears once its slug is active again.
    ///
    /// Not `#[cfg]`-gated: `local_scheduler` calls this on every platform
    /// (it has no Windows guard), so the function is live everywhere —
    /// unlike `checks()`, whose only caller is the Windows-only KLP
    /// evaluator.
    pub fn set_active_slugs(&self, slugs: HashSet<String>) {
        {
            let mut active = self
                .inner
                .active_slugs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *active = Some(Arc::new(slugs));
        }
        self.inner.updated.notify_one();
    }

    /// Current cached checks, filtered to those whose job is still live
    /// (see [`set_active_slugs`](Self::set_active_slugs)). Cheap clone of
    /// the small map's values; order is unspecified (the snapshot builder
    /// positions them). When the active set is still unknown (`None`,
    /// pre-first-resync) nothing is filtered.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn checks(&self) -> Vec<Check> {
        // Snapshot the active set first (and release its lock) so we
        // never hold both mutexes at once — `set_active_slugs` only ever
        // takes the active-set lock, so this ordering can't deadlock.
        let active = self
            .inner
            .active_slugs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        self.guarded(|map| {
            map.values()
                .filter(|c| match &active {
                    Some(set) => set.contains(c.name.as_str()),
                    None => true,
                })
                .cloned()
                .collect()
        })
    }

    /// Resolves the next time a check result is recorded.
    ///
    /// Relies on `tokio::sync::Notify` permit semantics: a
    /// `record()` → `notify_one()` that fires while no waiter is
    /// registered stores **one** permit, which the next `wait()`
    /// consumes immediately — so a result recorded between
    /// `eval_loop` iterations is never missed (multiple records
    /// coalesce into one wake, which is fine since the evaluator
    /// re-reads the whole map). Do NOT swap this for a condvar-style
    /// API that lacks the stored permit.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub async fn wait(&self) {
        self.inner.updated.notified().await;
    }

    /// Run `f` under the results lock, recovering the guard (with a
    /// warning) if a previous holder panicked — a poisoned lock here
    /// must not silently freeze or wipe the Health tab. The map's
    /// data is still structurally valid after a panic elsewhere.
    fn guarded<R>(&self, f: impl FnOnce(&mut HashMap<String, Check>) -> R) -> R {
        let mut map = self.inner.results.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("check_cache: results mutex poisoned — recovering");
            poisoned.into_inner()
        });
        f(&mut map)
    }
}

impl Default for CheckSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Merge operator-defined `extra` checks onto the agent's intrinsic
/// `base` checks: an operator check overrides a built-in of the same
/// name (so a fleet could shadow e.g. `agent_self_update` with its
/// own), and any remaining extras are appended. Pure; shared by
/// `eval_once`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn merge_checks(base: Vec<Check>, extra: &[Check]) -> Vec<Check> {
    let overridden: HashSet<&str> = extra.iter().map(|c| c.name.as_str()).collect();
    let mut merged: Vec<Check> = base
        .into_iter()
        .filter(|c| !overridden.contains(c.name.as_str()))
        .collect();
    merged.extend(extra.iter().cloned());
    merged
}

/// Map a check job's stdout JSON object + its [`CheckHint`] into a KLP
/// [`Check`]. Pure (no I/O) so it's unit-testable. Non-JSON / non-object
/// stdout, or a missing / unrecognised `status_field`, degrades to
/// [`CheckStatus::Unknown`] with a diagnostic detail — everything else
/// is up to the operator's PowerShell.
pub fn build_check(hint: &CheckHint, stdout: &str) -> Check {
    // #821: read check's own fenced block so a job can compose check with
    // inventory / collect (and/or a user message) on one stdout. No fence
    // ⇒ the whole stdout (back-compat for a check-only job). `fenced_
    // payload` already trims.
    let payload = kanade_shared::manifest::fenced_payload(
        stdout,
        kanade_shared::manifest::CHECK_BLOCK_BEGIN,
        kanade_shared::manifest::CHECK_BLOCK_END,
    );
    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => return unknown(hint, format!("check stdout was not JSON: {e}")),
    };
    let Some(obj) = value.as_object() else {
        return unknown(hint, "check stdout was not a JSON object".to_string());
    };
    let status = match obj.get(&hint.status_field) {
        Some(v) => match serde_json::from_value::<CheckStatus>(v.clone()) {
            Ok(s) => s,
            Err(_) => {
                return unknown(
                    hint,
                    format!(
                        "`{}` = {v} is not one of ok/warn/fail/unknown",
                        hint.status_field
                    ),
                );
            }
        },
        None => {
            return unknown(hint, format!("stdout has no `{}` field", hint.status_field));
        }
    };
    let detail = obj.get(&hint.detail_field).and_then(json_to_detail);
    Check {
        name: hint.name.clone(),
        label: hint.label.clone(),
        status,
        detail,
        troubleshoot: hint.troubleshoot.clone(),
    }
}

/// Build the [`Check`] for a `check:` job that exited non-zero. The
/// script crashed before it could report a status, so we surface
/// `Unknown` with the exit code + a stderr snippet rather than
/// leaving a stale `Ok` on the Health tab (a persistently-crashing
/// check must not read as healthy).
pub fn build_check_failed(hint: &CheckHint, exit_code: i32, stderr: &str) -> Check {
    let snippet: String = stderr.trim().chars().take(200).collect();
    let detail = if snippet.is_empty() {
        format!("check script exited {exit_code}")
    } else {
        format!("check script exited {exit_code}: {snippet}")
    };
    unknown(hint, detail)
}

fn unknown(hint: &CheckHint, detail: String) -> Check {
    Check {
        name: hint.name.clone(),
        label: hint.label.clone(),
        status: CheckStatus::Unknown,
        detail: Some(detail),
        troubleshoot: hint.troubleshoot.clone(),
    }
}

/// Render a detail field value as a string: pass non-empty strings
/// through, stringify scalars, and compact-JSON-encode arrays /
/// objects (so an operator who puts structured data in `detail` sees
/// *something* on the row instead of a silently-blank column). Only
/// `null` / empty-string yield `None`.
fn json_to_detail(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        // Array / Object → compact JSON, e.g. `["C:","D:"]`.
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(name: &str) -> CheckHint {
        CheckHint {
            name: name.into(),
            label: None,
            status_field: "status".into(),
            detail_field: "detail".into(),
            troubleshoot: None,
            fleet: true,
            alert: None,
        }
    }

    #[test]
    fn build_check_reads_status_and_detail() {
        let c = build_check(
            &hint("bitlocker"),
            r#"{"status":"warn","detail":"D: unprotected"}"#,
        );
        assert_eq!(c.name, "bitlocker");
        assert_eq!(c.status, CheckStatus::Warn);
        assert_eq!(c.detail.as_deref(), Some("D: unprotected"));
    }

    #[test]
    fn build_check_honours_custom_fields_and_troubleshoot() {
        let h = CheckHint {
            name: "patch".into(),
            label: Some("Windows 更新プログラム".into()),
            status_field: "compliance".into(),
            detail_field: "summary".into(),
            troubleshoot: Some("fix-patch".into()),
            fleet: true,
            alert: None,
        };
        let c = build_check(
            &h,
            r#"{"compliance":"fail","summary":"12 missing","extra":[1,2]}"#,
        );
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.label.as_deref(), Some("Windows 更新プログラム"));
        assert_eq!(c.detail.as_deref(), Some("12 missing"));
        assert_eq!(c.troubleshoot.as_deref(), Some("fix-patch"));
    }

    #[test]
    fn build_check_detail_optional() {
        let c = build_check(&hint("x"), r#"{"status":"ok"}"#);
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(c.detail.is_none());
    }

    #[test]
    fn build_check_degrades_on_bad_input() {
        // Not JSON.
        assert_eq!(
            build_check(&hint("x"), "not json").status,
            CheckStatus::Unknown
        );
        // Not an object.
        assert_eq!(
            build_check(&hint("x"), "[1,2,3]").status,
            CheckStatus::Unknown
        );
        // Missing status field.
        assert_eq!(
            build_check(&hint("x"), r#"{"detail":"hi"}"#).status,
            CheckStatus::Unknown
        );
        // Unrecognised status value — and the reason is surfaced.
        let bad = build_check(&hint("x"), r#"{"status":"green"}"#);
        assert_eq!(bad.status, CheckStatus::Unknown);
        assert!(bad.detail.unwrap().contains("green"));
    }

    #[test]
    fn merge_checks_overrides_builtin_by_name() {
        let base = vec![Check {
            name: "agent_self_update".into(),
            label: None,
            status: CheckStatus::Ok,
            detail: None,
            troubleshoot: None,
        }];
        let extra = vec![
            // operator's own check named like the built-in replaces it
            Check {
                name: "agent_self_update".into(),
                label: None,
                status: CheckStatus::Warn,
                detail: Some("override".into()),
                troubleshoot: None,
            },
            Check {
                name: "disk_space".into(),
                label: None,
                status: CheckStatus::Warn,
                detail: Some("8% free".into()),
                troubleshoot: None,
            },
            Check {
                name: "bitlocker".into(),
                label: None,
                status: CheckStatus::Ok,
                detail: None,
                troubleshoot: None,
            },
        ];
        let merged = merge_checks(base, &extra);
        // agent_self_update overridden (Warn, not duplicated); disk_space
        // + bitlocker appended.
        assert_eq!(merged.len(), 3);
        let asu = merged
            .iter()
            .find(|c| c.name == "agent_self_update")
            .unwrap();
        assert_eq!(asu.status, CheckStatus::Warn);
        assert!(merged.iter().any(|c| c.name == "bitlocker"));
        assert_eq!(
            merged
                .iter()
                .filter(|c| c.name == "agent_self_update")
                .count(),
            1
        );
    }

    #[test]
    fn build_check_failed_surfaces_exit_and_stderr() {
        let c = build_check_failed(&hint("av"), 1, "  Get-MpComputerStatus: access denied  ");
        assert_eq!(c.status, CheckStatus::Unknown);
        let d = c.detail.unwrap();
        assert!(d.contains("exited 1"), "detail: {d}");
        assert!(d.contains("access denied"), "detail: {d}");
    }

    #[test]
    fn json_to_detail_renders_arrays_as_compact_json() {
        let v: serde_json::Value = serde_json::json!(["C:", "D:"]);
        assert_eq!(json_to_detail(&v).as_deref(), Some(r#"["C:","D:"]"#));
        assert!(json_to_detail(&serde_json::Value::Null).is_none());
    }

    #[test]
    fn sink_record_and_read_round_trip() {
        let sink = CheckSink::new();
        assert!(sink.checks().is_empty());
        sink.record(build_check(&hint("bitlocker"), r#"{"status":"ok"}"#));
        let checks = sink.checks();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "bitlocker");
        assert_eq!(checks[0].status, CheckStatus::Ok);
    }

    // ---- active-slug filtering (deleted-job prune) ----

    #[test]
    fn checks_unfiltered_until_active_set_is_known() {
        // Before the scheduler's first resync the active set is `None`,
        // so every cached check shows — a broker-down boot must not
        // blank the Health tab.
        let sink = CheckSink::new();
        sink.record(build_check(&hint("bitlocker"), r#"{"status":"ok"}"#));
        sink.record(build_check(&hint("av_signature"), r#"{"status":"ok"}"#));
        assert_eq!(sink.checks().len(), 2);
    }

    #[test]
    fn active_slugs_prunes_checks_whose_job_was_deleted() {
        let sink = CheckSink::new();
        sink.record(build_check(&hint("bitlocker"), r#"{"status":"ok"}"#));
        sink.record(build_check(&hint("av_signature"), r#"{"status":"warn"}"#));

        // Job behind `av_signature` deleted → only `bitlocker` is still
        // a live job, so the tab shows just that one.
        sink.set_active_slugs(HashSet::from(["bitlocker".to_string()]));
        let names: Vec<String> = sink.checks().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["bitlocker".to_string()]);
    }

    #[test]
    fn active_slugs_empty_hides_all_but_keeps_data() {
        // Every check job deleted → nothing shows, but the cache still
        // holds the results (read-time filter, not a delete): restoring
        // the active set brings the rows back, no re-run needed.
        let sink = CheckSink::new();
        sink.record(build_check(&hint("bitlocker"), r#"{"status":"ok"}"#));
        sink.set_active_slugs(HashSet::new());
        assert!(sink.checks().is_empty());

        sink.set_active_slugs(HashSet::from(["bitlocker".to_string()]));
        assert_eq!(sink.checks().len(), 1);
    }

    #[tokio::test]
    async fn set_active_slugs_wakes_the_evaluator() {
        // A deletion must prune promptly (next push), not wait for the
        // 30 s tick — set_active_slugs stores a notify permit just like
        // record().
        let sink = CheckSink::new();
        sink.set_active_slugs(HashSet::new());
        tokio::time::timeout(std::time::Duration::from_secs(1), sink.wait())
            .await
            .expect("set_active_slugs must wake the evaluator");
    }

    #[tokio::test]
    async fn wait_resolves_when_a_result_is_recorded() {
        let sink = CheckSink::new();
        // notify_one stores a permit even though we record BEFORE
        // wait() is polled — the next wait() must still complete.
        sink.record(build_check(&hint("x"), r#"{"status":"ok"}"#));
        tokio::time::timeout(std::time::Duration::from_secs(1), sink.wait())
            .await
            .expect("wait() must observe the stored notify permit");
    }

    // ---- persistence (#290: survive restart / offline) ----

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let sink = CheckSink::load(dir.path().join("check_results.json"));
        assert!(sink.checks().is_empty());
    }

    #[test]
    fn record_persists_and_reloads_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check_results.json");

        // First "boot": record two checks.
        let sink = CheckSink::load(path.clone());
        sink.record(build_check(
            &hint("bitlocker"),
            r#"{"status":"warn","detail":"D: off"}"#,
        ));
        sink.record(build_check(&hint("av_signature"), r#"{"status":"ok"}"#));
        drop(sink);
        assert!(path.exists(), "record must persist the cache file");

        // Second "boot": a fresh sink loads the persisted results, so
        // the Health tab is populated before any check re-runs.
        let reloaded = CheckSink::load(path);
        let mut checks = reloaded.checks();
        checks.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "av_signature");
        assert_eq!(checks[1].name, "bitlocker");
        assert_eq!(checks[1].status, CheckStatus::Warn);
        assert_eq!(checks[1].detail.as_deref(), Some("D: off"));
    }

    #[test]
    fn load_corrupt_file_is_empty_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check_results.json");
        std::fs::write(&path, b"{ this is not valid json").unwrap();
        // Must boot with an empty cache rather than panicking.
        let sink = CheckSink::load(path);
        assert!(sink.checks().is_empty());
    }

    #[test]
    fn new_does_not_persist() {
        // The in-memory constructor writes nothing to disk.
        let sink = CheckSink::new();
        sink.record(build_check(&hint("x"), r#"{"status":"ok"}"#));
        assert_eq!(sink.checks().len(), 1);
        // (no path → persist() is a no-op; nothing to assert beyond
        // not panicking and the value still being in memory)
    }
}
