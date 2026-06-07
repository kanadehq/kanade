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
    /// after the cron edit. Kill of currently-running children is a
    /// separate op (`kanade kill <exec_job_id>`) for v0.27; full
    /// Layer 3 cascade lands in a later release.
    Disable {
        id: String,
        /// Also revoke the schedule's referenced Job so in-flight
        /// Commands skip on receipt.
        #[arg(long)]
        cascade: bool,
    },
}

pub async fn execute(backend_url: &str, args: ScheduleArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        ScheduleSub::Create { yaml } => create(base, &yaml).await,
        ScheduleSub::List => list(base).await,
        ScheduleSub::Delete { id } => delete(base, &id).await,
        ScheduleSub::Disable { id, cascade } => disable(base, &id, cascade).await,
    }
}

async fn create(base: &str, yaml: &PathBuf) -> Result<()> {
    let body = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // Parse client-side first so a malformed YAML errors at the
    // operator's shell rather than via the backend's 400 — keeps the
    // error site obvious. Then ship the raw YAML body so the
    // backend's BUCKET_SCHEDULES_YAML mirror preserves comments +
    // formatting across SPA edits.
    let schedule: Schedule =
        serde_yaml::from_str(&body).with_context(|| format!("parse {yaml:?}"))?;
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

async fn disable(base: &str, id: &str, cascade: bool) -> Result<()> {
    let url = format!("{base}/api/schedules/{id}/disable?cascade={cascade}");
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
    if cascade {
        println!("disabled (with cascade revoke): {id}");
    } else {
        println!("disabled: {id}");
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
