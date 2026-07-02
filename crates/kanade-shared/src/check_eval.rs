//! Derive a [`Check`] from a `check:` job's run output — the single
//! source of truth shared by the agent's Health-tab cache
//! (`kanade-agent::check_cache`) and the backend's fleet-wide
//! `check_status` projector, so the end user's Client App and the
//! operator SPA can never disagree about the same run (#908).
//!
//! Pure functions (no I/O) on purpose: both consumers feed them the
//! already-captured stdout/stderr of an ExecResult.

use crate::ipc::state::{Check, CheckStatus};
use crate::manifest::CheckHint;

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
    let payload = crate::manifest::fenced_payload(
        stdout,
        crate::manifest::CHECK_BLOCK_BEGIN,
        crate::manifest::CHECK_BLOCK_END,
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
        // gate-only checks (`health: false`) stay in the snapshot for
        // show_when but are hidden from the Client App's Health tab.
        health_hidden: !hint.health,
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
        // gate-only checks (`health: false`) stay in the snapshot for
        // show_when but are hidden from the Client App's Health tab.
        health_hidden: !hint.health,
    }
}

/// Render a detail field value as a string: pass non-empty strings
/// through, stringify scalars, and compact-JSON-encode arrays /
/// objects (so an operator who puts structured data in `detail` sees
/// *something* on the row instead of a silently-blank column). Only
/// `null` / empty-string yield `None`.
pub fn json_to_detail(v: &serde_json::Value) -> Option<String> {
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
            health: true,
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
            health: true,
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
    fn build_check_reads_fenced_payload() {
        // #821: a composed job fences check's payload so it can share
        // stdout with inventory / a human-readable message.
        let stdout = "updating...\n#KANADE-CHECK-BEGIN\n{\"status\":\"fail\",\"detail\":\"D: unprotected\"}\n#KANADE-CHECK-END\ndone.\n";
        let c = build_check(&hint("bitlocker"), stdout);
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(c.detail.as_deref(), Some("D: unprotected"));
    }

    #[test]
    fn build_check_sets_health_hidden_from_hint() {
        // Default hint (health = true) → shown on the Health tab.
        let shown = build_check(&hint("bitlocker"), r#"{"status":"ok"}"#);
        assert!(!shown.health_hidden);

        // A gate-only check (health = false) → recorded but hidden from
        // the Health tab; a crashed run (build_check_failed) keeps the flag.
        let mut h = hint("myapp-up-to-date");
        h.health = false;
        assert!(build_check(&h, r#"{"status":"fail"}"#).health_hidden);
        assert!(build_check_failed(&h, 1, "boom").health_hidden);
    }

    #[test]
    fn build_check_failed_surfaces_exit_and_stderr() {
        let c = build_check_failed(&hint("av"), 1, "  Get-MpComputerStatus: access denied  ");
        assert_eq!(c.status, CheckStatus::Unknown);
        let d = c.detail.unwrap();
        assert!(d.contains("exited 1"));
        assert!(d.contains("access denied"));
    }

    #[test]
    fn json_to_detail_renders_arrays_as_compact_json() {
        let v: serde_json::Value = serde_json::json!(["C:", "D:"]);
        assert_eq!(json_to_detail(&v).as_deref(), Some(r#"["C:","D:"]"#));
        assert!(json_to_detail(&serde_json::Value::Null).is_none());
    }
}
