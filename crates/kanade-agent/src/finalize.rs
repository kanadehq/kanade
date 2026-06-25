//! Job-generic post-step hook (`finalize:`). Runs AFTER the main script
//! (and, for a `collect:` job, after the bundle upload) on a clean exit,
//! with the step's structured result injected as an environment variable
//! so the hook can delete / move / notify. Best-effort: any failure is
//! logged and never published as the run's outcome — the upload, if any,
//! already succeeded.
//!
//! The result is injected by **prepending a shell assignment** to the
//! hook body rather than threading an env var through the spawn paths.
//! That keeps the System / User / SystemGui launch code untouched and
//! makes the var available identically regardless of `run_as`.

use async_nats::Client;
use kanade_shared::wire::{Command, FinalizeCommand, Shell};
use serde_json::json;
use tracing::{info, warn};

use crate::process::{ExecOutcome, run_command_with_kill};

/// Build the `KANADE_COLLECT_RESULT` JSON for a collect job's finalize
/// hook from the uploaded bundles. Each entry carries its `label` (None
/// for the single-bundle form), Object Store `key`, and the `files`
/// actually packed. An empty slice (no collect hint, or every upload
/// failed) yields `{ "ok": false, "bundles": [] }`, so a hook that only
/// acts on `uploaded` files touches nothing.
pub fn collect_result_json(bundles: &[crate::collect::BundleResult]) -> String {
    let arr: Vec<_> = bundles
        .iter()
        .map(|b| {
            json!({
                "label": b.label,
                "key": b.key,
                "uploaded": true,
                "files": b.files,
            })
        })
        .collect();
    json!({ "ok": !arr.is_empty(), "bundles": arr }).to_string()
}

/// Build the PowerShell prelude that injects `result_json` as
/// `KANADE_COLLECT_RESULT`. A single-quoted PowerShell string treats only
/// `'` as special (escaped by doubling); serde's JSON is single-line, so
/// there are no raw newlines to handle. This is the security-sensitive
/// step — kept as its own function so the escaping is unit-tested. (cmd
/// is rejected upstream, so only the PowerShell form exists.)
fn powershell_prelude(result_json: &str) -> String {
    format!(
        "$env:KANADE_COLLECT_RESULT = '{}'\n",
        result_json.replace('\'', "''")
    )
}

/// Run the `finalize:` hook best-effort. `result_json` is injected as
/// `KANADE_COLLECT_RESULT`. Reuses [`run_command_with_kill`] — the same
/// staging / kill / timeout machinery the main script uses.
pub async fn run_finalize(
    client: &Client,
    parent: &Command,
    fin: &FinalizeCommand,
    result_json: Option<&str>,
) {
    // Defense in depth: `Manifest::validate` already rejects a cmd
    // finalize at the write boundary. Bail here too so a Command that
    // somehow carried `Shell::Cmd` (validation bypass, downgraded wire)
    // never reaches the unsafe injection path — cmd.exe quoting doesn't
    // nest, so JSON `"` + shell metacharacters in a collected path could
    // break out into command injection at the agent's privilege.
    if matches!(fin.shell, Shell::Cmd) {
        warn!(job = %parent.id, "finalize: cmd shell is not supported (injection risk); skipping hook");
        return;
    }
    // Inject `KANADE_COLLECT_RESULT` only when there's a collect payload
    // (a `collect:` job). A non-collect finalize hook sees no such
    // variable, rather than a synthetic `{"ok":false,"bundles":[]}` —
    // which would be an observable contract break for callers that key
    // off the variable's presence.
    let prelude = result_json.map(powershell_prelude).unwrap_or_default();
    let script = format!("{prelude}{}", fin.script);

    // A synthetic Command reusing the parent's identity/kill subject but
    // carrying NO hints (no recursion into collect/finalize) — the hook
    // is a plain script run.
    let fin_cmd = Command {
        id: format!("{}__finalize", parent.id),
        version: parent.version.clone(),
        request_id: parent.request_id.clone(),
        exec_id: parent.exec_id.clone(),
        shell: fin.shell,
        script,
        script_object: None,
        script_object_sha256: None,
        timeout_secs: fin.timeout_secs,
        jitter_secs: None,
        run_as: fin.run_as,
        cwd: fin.cwd.clone(),
        deadline_at: None,
        staleness: parent.staleness.clone(),
        emit: None,
        check: None,
        collect: None,
        retry: None,
        finalize: None,
    };

    match run_command_with_kill(client, &fin_cmd, None).await {
        Ok(ExecOutcome::Completed { exit_code: 0, .. }) => {
            info!(job = %parent.id, "finalize: hook completed");
        }
        Ok(ExecOutcome::Completed {
            exit_code, stderr, ..
        }) => {
            warn!(job = %parent.id, exit_code, stderr = %stderr, "finalize: hook exited non-zero (ignored)");
        }
        Ok(ExecOutcome::Killed { .. }) => {
            warn!(job = %parent.id, "finalize: hook killed (ignored)");
        }
        Ok(ExecOutcome::Timeout { .. }) => {
            warn!(job = %parent.id, "finalize: hook timed out (ignored)");
        }
        Err(e) => {
            warn!(job = %parent.id, error = %e, "finalize: hook spawn failed (ignored)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_json_marks_uploaded_when_collected() {
        let json = collect_result_json(&[
            crate::collect::BundleResult {
                label: Some("20260101".into()),
                key: "pc/job/20260101__ts.zip".into(),
                files: vec!["C:/a.png".into(), "C:/b.png".into()],
            },
            crate::collect::BundleResult {
                label: Some("20260102".into()),
                key: "pc/job/20260102__ts.zip".into(),
                files: vec!["C:/c.png".into()],
            },
        ]);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["bundles"].as_array().unwrap().len(), 2);
        assert_eq!(v["bundles"][0]["label"], "20260101");
        assert_eq!(v["bundles"][0]["key"], "pc/job/20260101__ts.zip");
        assert_eq!(v["bundles"][0]["uploaded"], true);
        assert_eq!(v["bundles"][0]["files"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn powershell_prelude_doubles_single_quotes() {
        // A collected path containing `'` must be escaped by doubling so
        // it can't break out of the single-quoted assignment.
        let p = powershell_prelude(r#"{"files":["C:/it's here.png"]}"#);
        assert!(
            p.contains("it''s here"),
            "single quote must be doubled: {p}"
        );
        assert!(p.starts_with("$env:KANADE_COLLECT_RESULT = '"));
        assert!(p.ends_with("'\n"));
        // No unescaped lone single quote remains inside the value.
        let inner = p
            .trim_start_matches("$env:KANADE_COLLECT_RESULT = '")
            .trim_end_matches("'\n");
        assert!(!inner.contains('\'') || inner.contains("''"));
    }

    #[test]
    fn result_json_is_empty_when_nothing_collected() {
        let json = collect_result_json(&[]);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["bundles"].as_array().unwrap().len(), 0);
    }
}
