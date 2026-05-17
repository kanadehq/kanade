use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use kanade_shared::manifest::Manifest;
use serde::Deserialize;
use tracing::info;

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Path to the job YAML manifest (spec §2.4.1).
    pub yaml: PathBuf,
    /// Override the manifest's version (handy for hot-patches without editing the YAML).
    #[arg(long)]
    pub version: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ExecResponse {
    exec_id: String,
    job_id: String,
    version: String,
    target_count: u32,
    subjects: Vec<String>,
}

pub async fn execute(backend_url: &str, args: ExecArgs) -> Result<()> {
    let yaml =
        std::fs::read_to_string(&args.yaml).with_context(|| format!("read {:?}", args.yaml))?;
    let mut manifest: Manifest =
        serde_yaml::from_str(&yaml).with_context(|| format!("parse {:?}", args.yaml))?;
    if let Some(v) = args.version {
        manifest.version = v;
    }
    info!(job_id = %manifest.id, version = %manifest.version, "executing");

    let url = format!("{}/api/exec", backend_url.trim_end_matches('/'));
    let client = crate::http_client::authed_client()?;
    let resp = client
        .post(&url)
        .json(&manifest)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("exec rejected: {status} — {body}");
    }
    let payload: ExecResponse = resp.json().await.context("decode exec response")?;

    println!("exec_id       : {}", payload.exec_id);
    println!("job_id        : {}", payload.job_id);
    println!("version       : {}", payload.version);
    println!("target_count  : {}", payload.target_count);
    println!("subjects      :");
    for s in &payload.subjects {
        println!("  - {s}");
    }
    Ok(())
}
