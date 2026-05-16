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
        /// Path to the schedule YAML (`id` / `cron` / `manifest` / `enabled`).
        yaml: PathBuf,
    },
    /// List all schedules currently stored in the schedules KV.
    List,
    /// Delete a schedule by its id.
    Delete { id: String },
}

pub async fn execute(backend_url: &str, args: ScheduleArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        ScheduleSub::Create { yaml } => create(base, &yaml).await,
        ScheduleSub::List => list(base).await,
        ScheduleSub::Delete { id } => delete(base, &id).await,
    }
}

async fn create(base: &str, yaml: &PathBuf) -> Result<()> {
    let body = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    let schedule: Schedule =
        serde_yaml::from_str(&body).with_context(|| format!("parse {yaml:?}"))?;
    info!(schedule_id = %schedule.id, cron = %schedule.cron, "upserting schedule");

    let url = format!("{base}/api/schedules");
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .json(&schedule)
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
