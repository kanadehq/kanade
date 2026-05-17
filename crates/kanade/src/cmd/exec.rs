//! `kanade exec <job-id>` — fan out a registered job to its declared
//! targets. The Manifest body lives in the catalog (`kanade job
//! create`), so this command takes only the job's id; the backend
//! resolves it server-side and 404s if it's not registered.

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;
use tracing::info;

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Id of a registered job (see `kanade job list`). Manifests
    /// are no longer accepted inline — register first with
    /// `kanade job create <manifest.yaml>`.
    pub job_id: String,
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
    info!(job_id = %args.job_id, "executing");

    let url = format!(
        "{}/api/exec/{}",
        backend_url.trim_end_matches('/'),
        args.job_id,
    );
    let resp = crate::http_client::authed_client()?
        .post(&url)
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
