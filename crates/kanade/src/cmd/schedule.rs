use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::Schedule;
use tracing::info;

#[derive(Args, Debug)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub sub: ScheduleSub,
}

#[derive(Subcommand, Debug)]
pub enum ScheduleSub {
    /// Upsert a schedule from a YAML file.
    Create {
        /// Path to the schedule YAML (`id` / `when` / `job_id` / `enabled`).
        /// The referenced job must already be registered via `kanade job create`.
        yaml: PathBuf,
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
}

pub async fn execute(backend_url: &str, args: ScheduleArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        ScheduleSub::Create { yaml } => create(base, &yaml).await,
        ScheduleSub::List => list(base).await,
        ScheduleSub::Delete { id } => delete(base, &id).await,
        ScheduleSub::Disable {
            id,
            cascade,
            cascade_kill,
        } => disable(base, &id, cascade, cascade_kill).await,
        ScheduleSub::Preview { id, count } => preview(base, &id, count).await,
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

async fn create(base: &str, yaml: &PathBuf) -> Result<()> {
    let body = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
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
    let payload: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
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
