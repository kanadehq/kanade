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
}

impl CheckSink {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                results: Mutex::new(HashMap::new()),
                updated: Notify::new(),
            }),
        }
    }

    /// Store a freshly-built [`Check`] under its `name` and wake the
    /// state evaluator. The caller builds the `Check` — [`build_check`]
    /// on a clean exit, [`build_check_failed`] when the script crashed
    /// — so this stays a dumb sink.
    pub fn record(&self, check: Check) {
        self.guarded(|map| {
            map.insert(check.name.clone(), check);
        });
        // Re-publish the snapshot now instead of on the next 30 s tick.
        self.inner.updated.notify_one();
    }

    /// Current cached checks. Cheap clone of the small map's values;
    /// order is unspecified (the snapshot builder positions them).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn checks(&self) -> Vec<Check> {
        self.guarded(|map| map.values().cloned().collect())
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
/// name (so a fleet can replace e.g. `disk_free` with its own), and
/// any remaining extras are appended. Pure; shared by `eval_once`.
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
    let value: serde_json::Value = match serde_json::from_str(stdout.trim()) {
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
            status_field: "status".into(),
            detail_field: "detail".into(),
            troubleshoot: None,
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
            status_field: "compliance".into(),
            detail_field: "summary".into(),
            troubleshoot: Some("fix-patch".into()),
        };
        let c = build_check(
            &h,
            r#"{"compliance":"fail","summary":"12 missing","extra":[1,2]}"#,
        );
        assert_eq!(c.status, CheckStatus::Fail);
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
        let base = vec![
            Check {
                name: "agent_self_update".into(),
                status: CheckStatus::Ok,
                detail: None,
                troubleshoot: None,
            },
            Check {
                name: "disk_free".into(),
                status: CheckStatus::Ok,
                detail: None,
                troubleshoot: None,
            },
        ];
        let extra = vec![
            // operator's own disk_free replaces the built-in
            Check {
                name: "disk_free".into(),
                status: CheckStatus::Warn,
                detail: Some("90%".into()),
                troubleshoot: None,
            },
            Check {
                name: "bitlocker".into(),
                status: CheckStatus::Ok,
                detail: None,
                troubleshoot: None,
            },
        ];
        let merged = merge_checks(base, &extra);
        // agent_self_update kept, disk_free overridden (Warn), bitlocker added.
        assert_eq!(merged.len(), 3);
        let disk = merged.iter().find(|c| c.name == "disk_free").unwrap();
        assert_eq!(disk.status, CheckStatus::Warn);
        assert!(merged.iter().any(|c| c.name == "bitlocker"));
        assert_eq!(merged.iter().filter(|c| c.name == "disk_free").count(), 1);
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
}
