use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::Schedule;
use tracing::{info, warn};

use crate::cmd::provenance::{append_origin_yaml, detect_repo_origin, has_top_level_origin};

#[derive(Args, Debug)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub sub: ScheduleSub,
}

#[derive(Subcommand, Debug)]
pub enum ScheduleSub {
    /// Upsert one or more schedules from YAML files.
    ///
    /// Accepts multiple files, a directory (its top-level `*.yaml` /
    /// `*.yml`), and/or glob patterns — e.g. `kanade schedule create
    /// configs/schedules/*.yaml`. Each file is registered independently
    /// (fail-soft per file); the command exits non-zero if any fails.
    /// The referenced job must already be registered via `kanade job
    /// create`.
    Create {
        /// Schedule YAML paths (`id` / `when` / `job_id` / `enabled`).
        /// Globs / directories are expanded CLI-side, so quote a glob to
        /// keep your shell from expanding it first.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Validate one or more schedule manifests WITHOUT submitting them.
    ///
    /// Runs the exact client-side checks `create` does — strict YAML
    /// parse (#492) + `Schedule::validate()` (when/runs_on combos, cron +
    /// humantime parsing, active-window / constraints checks) — but never
    /// contacts the backend. Two caveats, both shared with `create` (so
    /// validate has parity, not a weaker check): the referenced `job_id`
    /// existence is a backend-side check and is NOT verified here; and
    /// unknown *top-level* keys are NOT rejected, because `Schedule`
    /// flattens `FanoutPlan` and serde's `flatten` defeats the strict
    /// parser's unknown-key detection (nested unknown keys still are).
    /// Built for CI / pre-commit hooks: `kanade schedule validate
    /// configs/schedules/*.yaml`. Accepts files, directories, and globs
    /// like `create`; exits non-zero if any file fails.
    Validate {
        /// Schedule YAML paths to check. Globs / directories are expanded
        /// CLI-side, so quote a glob to keep your shell from expanding it
        /// first.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Export registered schedule YAML (the comment-preserving mirror).
    ///
    /// `kanade schedule export <id>` prints to stdout; with `--out-dir`
    /// it writes `<dir>/<id>.yaml`. `--all --out-dir <dir>` dumps every
    /// registered schedule. Round-trips with `create`.
    Export {
        /// Schedule id to export. Omit only with `--all`.
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Export every registered schedule (requires `--out-dir`).
        #[arg(long, conflicts_with = "id", requires = "out_dir")]
        all: bool,
        /// Directory to write `<id>.yaml` into. Required with `--all`;
        /// for a single id, omit it to print to stdout instead.
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// List all schedules currently stored in the schedules KV.
    List,
    /// Delete a schedule by its id.
    Delete { id: String },
    /// v0.27: stop a schedule from firing further ticks (SPEC §2.6.4 (c)).
    ///
    /// Soft disable (default): flip `enabled = false` so the cron loop
    /// — backend scheduler + agent local_scheduler both — stops on the
    /// next watch tick. Already-fired Commands run to completion.
    ///
    /// Hard disable (`--cascade`): soft disable PLUS Layer 2 cascade
    /// revoke of the underlying Job, so any in-flight Command for
    /// `schedule.job_id` gets skipped by the agent's `handle_command`
    /// KV check. Useful when an active rollout needs to stop NOW and
    /// you don't want stragglers running on offline agents reconnecting
    /// after the cron edit.
    ///
    /// `--cascade-kill` additionally publishes `kill.{exec_id}` for
    /// every still-running exec of the job (Layer 3), terminating
    /// currently-executing children. Orthogonal to `--cascade`: kill
    /// stops *running* work, revoke stops *queued/future* work — pass
    /// both for a full hard-disable. Kill is online-only (can't reach
    /// an offline agent's child) and destructive, so it's a separate
    /// explicit opt-in.
    Disable {
        id: String,
        /// Also revoke the schedule's referenced Job so in-flight
        /// Commands skip on receipt (Layer 2).
        #[arg(long)]
        cascade: bool,
        /// Also kill currently-running children of the job (Layer 3).
        /// Online-only + destructive — combine with `--cascade` to also
        /// stop queued/future runs.
        #[arg(long)]
        cascade_kill: bool,
    },
    /// Dry-run a schedule: print its next fire times (#418).
    ///
    /// Calendar schedules (`at` / `days`) print the next `--count`
    /// wall-clock fires, resolved in the schedule's tz and honoring its
    /// `active` window + `constraints.window` — so you can confirm a
    /// `tue#2` / `friL` / overnight-window schedule does what you think
    /// BEFORE deploying it. Reconcile shapes (`per_pc` / `per_target`)
    /// poll every minute, so they print their cadence instead of
    /// discrete times.
    Preview {
        id: String,
        /// How many upcoming fires to list (1..=50, default 5). Rejected
        /// at parse time when out of range rather than silently clamped.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..=50))]
        count: u8,
    },
    /// Coverage view for a schedule (#418): enabled, next fire, last
    /// run, and a 24h ok/fail tally.
    ///
    /// `next_run` comes from the same engine as `preview`; the run
    /// figures come from `execution_results` keyed by the schedule's
    /// `job_id` (so two schedules sharing a job share these numbers).
    Status { id: String },
    /// Rollout coverage (#418): of the schedule's FULL target roster
    /// (offline hosts included), how many have completed-ok / failed /
    /// are running / are still pending — plus the manifest version each
    /// agent last ran. Built for tracking a fleet-wide rollout (e.g. a
    /// vuln-fix app upgrade) to completion.
    ///
    /// By default only the not-yet-done agents (fail / running /
    /// pending) are listed; pass `--all` to list every targeted host.
    Coverage {
        id: String,
        /// List every targeted host, not just the not-yet-done ones.
        #[arg(long)]
        all: bool,
    },
}

pub async fn execute(backend_url: &str, args: ScheduleArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        ScheduleSub::Create { paths } => create_all(base, paths).await,
        // Offline check — no backend round-trip, so `base` is unused here.
        ScheduleSub::Validate { paths } => validate_all(paths),
        ScheduleSub::Export { id, all, out_dir } => {
            crate::cmd::bulk::export(base, "schedules", id, all, out_dir).await
        }
        ScheduleSub::List => list(base).await,
        ScheduleSub::Delete { id } => delete(base, &id).await,
        ScheduleSub::Disable {
            id,
            cascade,
            cascade_kill,
        } => disable(base, &id, cascade, cascade_kill).await,
        ScheduleSub::Preview { id, count } => preview(base, &id, count).await,
        ScheduleSub::Status { id } => status(base, &id).await,
        ScheduleSub::Coverage { id, all } => coverage(base, &id, all).await,
    }
}

async fn preview(base: &str, id: &str, count: u8) -> Result<()> {
    let url = format!("{base}/api/schedules/{id}/preview?count={count}");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("preview failed: {status} — {body}");
    }
    let p: serde_json::Value = resp.json().await?;
    let when = p.get("when").and_then(|v| v.as_str()).unwrap_or("?");
    let tz = p.get("tz").and_then(|v| v.as_str()).unwrap_or("?");
    let disabled = if p.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        "  [DISABLED]"
    } else {
        ""
    };
    println!("{id}  —  {when}  (tz: {tz}){disabled}");
    match p.get("fires").and_then(|v| v.as_array()) {
        Some(f) if !f.is_empty() => {
            for (i, t) in f.iter().enumerate() {
                println!("  {:>2}. {}", i + 1, t.as_str().unwrap_or("?"));
            }
        }
        _ => {
            let note = p
                .get("note")
                .and_then(|v| v.as_str())
                .unwrap_or("no upcoming fires");
            println!("  (none) {note}");
        }
    }
    Ok(())
}

async fn status(base: &str, id: &str) -> Result<()> {
    let url = format!("{base}/api/schedules/{id}/status");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("status failed: {s} — {body}");
    }
    let p: serde_json::Value = resp.json().await?;
    let when = p.get("when").and_then(|v| v.as_str()).unwrap_or("?");
    let tz = p.get("tz").and_then(|v| v.as_str()).unwrap_or("?");
    let disabled = if p.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        "  [DISABLED]"
    } else {
        ""
    };
    println!("{id}  —  {when}  (tz: {tz}){disabled}");
    println!(
        "  next run : {}",
        p.get("next_run").and_then(|v| v.as_str()).unwrap_or("—")
    );
    match p.get("last_run") {
        Some(lr) if !lr.is_null() => {
            let pc = lr.get("pc_id").and_then(|v| v.as_str()).unwrap_or("?");
            let fin = lr.get("finished_at").and_then(|v| v.as_str());
            let outcome = match (fin, lr.get("exit_code").and_then(|v| v.as_i64())) {
                (None, _) => "running".to_string(),
                (Some(_), Some(0)) => "ok".to_string(),
                (Some(_), Some(code)) => format!("exit {code}"),
                (Some(_), None) => "done".to_string(),
            };
            let at = lr
                .get("finished_at")
                .and_then(|v| v.as_str())
                .or_else(|| lr.get("started_at").and_then(|v| v.as_str()))
                .unwrap_or("?");
            println!("  last run : {at}  on {pc}  ({outcome})");
        }
        _ => println!("  last run : (never)"),
    }
    if let Some(rec) = p.get("recent") {
        let h = rec
            .get("window_hours")
            .and_then(|v| v.as_i64())
            .unwrap_or(24);
        let ok = rec.get("ok").and_then(|v| v.as_i64()).unwrap_or(0);
        let fail = rec.get("fail").and_then(|v| v.as_i64()).unwrap_or(0);
        println!("  last {h}h : {ok} ok / {fail} fail");
    }
    Ok(())
}

async fn coverage(base: &str, id: &str, all: bool) -> Result<()> {
    let url = format!("{base}/api/schedules/{id}/coverage");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("coverage failed: {s} — {body}");
    }
    let p: serde_json::Value = resp.json().await?;
    let when = p.get("when").and_then(|v| v.as_str()).unwrap_or("?");
    let job = p.get("job_id").and_then(|v| v.as_str()).unwrap_or("?");
    let runs_on = p.get("runs_on").and_then(|v| v.as_str()).unwrap_or("?");
    let n = |k: &str| p.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let (total, ok, fail, running, pending) =
        (n("total"), n("ok"), n("fail"), n("running"), n("pending"));
    println!("{id}  —  {when}  (job: {job}, runs_on: {runs_on})");
    println!("  rollout : {ok}/{total} ok · {fail} fail · {running} running · {pending} pending");

    // Per-agent detail: not-yet-done by default (fail/running/pending),
    // everything with --all. ok rows are quiet unless --all.
    let show = |state: &str| all || state != "ok";
    if let Some(agents) = p.get("agents").and_then(|v| v.as_array()) {
        let mut shown = 0usize;
        for a in agents {
            let state = a.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            if !show(state) {
                continue;
            }
            let pc = a.get("pc_id").and_then(|v| v.as_str()).unwrap_or("?");
            let ver = a.get("version").and_then(|v| v.as_str()).unwrap_or("—");
            println!("  {pc:<24} {state:<8} {ver}");
            shown += 1;
        }
        if shown == 0 {
            println!(
                "  {}",
                if all {
                    "(no targeted agents)"
                } else {
                    "(all targeted agents done)"
                }
            );
        }
    }
    Ok(())
}

/// Expand the operator's `create` arguments (files / dirs / globs) and
/// upsert each schedule. Fail-soft per file so one bad schedule in a
/// batch doesn't abort the rest; the command still exits non-zero if any
/// file failed (#654).
async fn create_all(base: &str, paths: Vec<PathBuf>) -> Result<()> {
    let files = crate::cmd::bulk::expand_manifest_paths(&paths)?;
    let mut failures = 0usize;
    for f in &files {
        if let Err(e) = create_one(base, f).await {
            eprintln!("✗ {}: {e:#}", f.display());
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures}/{} schedule manifest(s) failed", files.len());
    }
    Ok(())
}

/// Expand the operator's `validate` arguments (files / dirs / globs) and
/// check each schedule without submitting it. Fail-soft per file so one
/// bad manifest in a batch doesn't hide the rest; exits non-zero if any
/// file failed so the command gates CI.
fn validate_all(paths: Vec<PathBuf>) -> Result<()> {
    let files = crate::cmd::bulk::expand_manifest_paths(&paths)?;
    let mut failures = 0usize;
    for f in &files {
        if let Err(e) = validate_one(f) {
            eprintln!("✗ {}: {e:#}", f.display());
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!(
            "{failures}/{} schedule manifest(s) failed validation",
            files.len(),
        );
    }
    Ok(())
}

/// Parse + validate one schedule manifest WITHOUT submitting it. Mirrors
/// the client-side checks `create_one` runs up to (but not including)
/// the HTTP POST: strict parse → `Schedule::validate()`. The `job_id`
/// existence check is backend-owned and intentionally not done here.
fn validate_one(yaml: &std::path::Path) -> Result<()> {
    let body = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // #492: strict parse so unknown keys (operator typos) are rejected
    // with their paths, exactly as `create` would reject them.
    let schedule: Schedule = kanade_shared::strict::from_yaml_str(&body)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;
    schedule
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid schedule {yaml:?}: {e}"))?;
    println!(
        "✓ {} → schedule '{}' ({}) (valid)",
        yaml.display(),
        schedule.id,
        schedule.when,
    );
    Ok(())
}

async fn create_one(base: &str, yaml: &std::path::Path) -> Result<()> {
    // `mut` because the #695 provenance step below appends an `origin:`
    // block to the raw YAML before it is shipped.
    let mut body = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // Parse client-side first so a malformed YAML errors at the
    // operator's shell rather than via the backend's 400 — keeps the
    // error site obvious. Then ship the raw YAML body so the
    // backend's BUCKET_SCHEDULES_YAML mirror preserves comments +
    // formatting across SPA edits.
    // #492: strict parse — unknown keys are operator typos at this
    // boundary; fleet-side reads of the same type stay tolerant.
    let schedule: Schedule = kanade_shared::strict::from_yaml_str(&body)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;
    // Same client-side-first rationale for the semantic checks
    // (#418 decision F): a per_target+agent combo or a bad `every`
    // fails right here instead of as the backend's 400. The backend
    // re-validates anyway (and owns the job_id-exists check).
    schedule
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid schedule {yaml:?}: {e}"))?;
    info!(
        schedule_id = %schedule.id,
        when = %schedule.when,
        job_id = %schedule.job_id,
        "upserting schedule",
    );

    // #695: GitOps provenance — parity with `kanade job create` (#678).
    // Stamp the schedule with its repo-relative path when applied from a
    // Git/jj work tree, so the SPA renders it read-only and points edits
    // back at the repo instead of letting a ClickOps edit silently
    // diverge from Git. Schedules carry no script, so the script_file arg
    // is always `None`. Append to the raw body the CLI already ships, so
    // the backend's BUCKET_SCHEDULES_YAML mirror carries it too.
    if let Some(origin) = detect_repo_origin(yaml, None) {
        if has_top_level_origin(&body) {
            warn!(
                schedule_id = %schedule.id,
                "origin: already present in source YAML; preserving it. \
                 If the repo / remote changed, delete + recreate the schedule \
                 to refresh provenance",
            );
        } else {
            append_origin_yaml(&mut body, &origin).context("append origin provenance")?;
        }
    }

    let url = format!("{base}/api/schedules");
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/yaml")
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("create rejected: {status} — {body}");
    }
    // Concise per-file line so a bulk `create` stays digestible (#654).
    // Summary payload is `{id, when, job_id, enabled}`.
    let payload: serde_json::Value = resp
        .json()
        .await
        .context("parse JSON response from server")?;
    let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let when = payload.get("when").and_then(|v| v.as_str()).unwrap_or("?");
    println!("✓ {} → schedule '{id}' ({when})", yaml.display());
    Ok(())
}

async fn list(base: &str) -> Result<()> {
    let url = format!("{base}/api/schedules");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("list failed: {}", resp.status());
    }
    let payload: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

async fn disable(base: &str, id: &str, cascade: bool, cascade_kill: bool) -> Result<()> {
    let url =
        format!("{base}/api/schedules/{id}/disable?cascade={cascade}&cascade_kill={cascade_kill}");
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("disable failed: {status} — {body}");
    }
    match (cascade, cascade_kill) {
        (true, true) => println!("disabled (cascade revoke + kill in-flight): {id}"),
        (true, false) => println!("disabled (with cascade revoke): {id}"),
        (false, true) => println!("disabled (kill in-flight only): {id}"),
        (false, false) => println!("disabled: {id}"),
    }
    Ok(())
}

async fn delete(base: &str, id: &str) -> Result<()> {
    let url = format!("{base}/api/schedules/{id}");
    let resp = crate::http_client::authed_client()?
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("delete failed: {status} — {body}");
    }
    println!("deleted: {id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repo root two levels up from `crates/kanade`. The real-config
    /// test skips in a sourceless sandbox.
    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/kanade has a repo root two levels up")
            .to_path_buf()
    }

    #[test]
    fn validate_one_accepts_a_real_schedule() {
        let yaml = repo_root().join("configs/schedules/check-av-signature.yaml");
        if !yaml.exists() {
            return;
        }
        validate_one(&yaml).expect("real schedule validates offline");
    }

    #[test]
    fn validate_one_rejects_semantic_failure() {
        // Parses cleanly but fails `Schedule::validate()` — a bad
        // humantime `jitter` — so this proves validate runs the semantic
        // layer offline, not just the YAML parse. (Unknown top-level keys
        // are NOT a reliable negative here: `Schedule` flattens
        // `FanoutPlan`, and serde flatten defeats the strict parser's
        // serde_ignored unknown-key detection — same gap `create` has.)
        let dir = tempfile::tempdir().expect("mk temp dir");
        let yaml_path = dir.path().join("schedule.yaml");
        std::fs::write(
            &yaml_path,
            "id: s\nwhen:\n  per_pc: { every: 2h }\njob_id: j\nenabled: true\njitter: 5x\n",
        )
        .expect("write temp schedule");
        let err = validate_one(&yaml_path).expect_err("bad jitter must fail validate");
        assert!(
            format!("{err:#}").contains("jitter"),
            "expected a jitter validation error, got: {err:#}",
        );
    }
}
