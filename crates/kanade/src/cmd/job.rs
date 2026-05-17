//! `kanade job` — manage the job catalog (BUCKET_JOBS).
//!
//! A registered Job is just a [`Manifest`] keyed by its `id`.
//! Schedules and ad-hoc deploys reference it by id; editing a job
//! in-place rewrites what subsequent fires deploy.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::Manifest;
use tracing::info;

#[derive(Args, Debug)]
pub struct JobArgs {
    #[command(subcommand)]
    pub sub: JobSub,
}

#[derive(Subcommand, Debug)]
pub enum JobSub {
    /// Upsert a job into the catalog from a YAML manifest.
    Create {
        /// Path to the job YAML (Manifest body — `id` / `version` /
        /// `target` / `execute` / optional `inventory`).
        yaml: PathBuf,
    },
    /// List every job in the catalog.
    List,
    /// Delete a job by id. Refuses when any schedule references it.
    Delete { id: String },
}

pub async fn execute(backend_url: &str, args: JobArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        JobSub::Create { yaml } => create(base, &yaml).await,
        JobSub::List => list(base).await,
        JobSub::Delete { id } => delete(base, &id).await,
    }
}

async fn create(base: &str, yaml: &PathBuf) -> Result<()> {
    let body = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    let job: Manifest = serde_yaml::from_str(&body).with_context(|| format!("parse {yaml:?}"))?;
    info!(job_id = %job.id, version = %job.version, "upserting job");

    let url = format!("{base}/api/jobs");
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .json(&job)
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
    let url = format!("{base}/api/jobs");
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
    let url = format!("{base}/api/jobs/{id}");
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
