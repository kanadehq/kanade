//! `kanade exec <job-id>` — fan out a registered job to a
//! caller-specified target. v0.18: target lives on the caller now
//! (not on the Manifest), so flags pick `--all` / `--groups` /
//! `--pcs`. Wave rollout is intentionally not exposed on the CLI —
//! use a Schedule yaml if you want waves.

use anyhow::{Context, Result};
use clap::Args;
use kanade_shared::manifest::{FanoutPlan, Target};
use serde::Deserialize;
use tracing::info;

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Id of a registered job (see `kanade job list`).
    pub job_id: String,

    /// Fire at every agent. Mutually exclusive with --groups / --pcs.
    #[arg(long, conflicts_with_all = ["groups", "pcs"])]
    pub all: bool,

    /// Comma-separated group names (e.g. `--groups canary,wave1`).
    #[arg(long, value_delimiter = ',')]
    pub groups: Vec<String>,

    /// Comma-separated pc_ids (e.g. `--pcs minipc-01,minipc-02`).
    #[arg(long, value_delimiter = ',')]
    pub pcs: Vec<String>,

    /// Humantime jitter window the agent randomises start within
    /// (e.g. `30s`, `5m`).
    #[arg(long)]
    pub jitter: Option<String>,
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
    let target = Target {
        all: args.all,
        groups: args.groups,
        pcs: args.pcs,
    };
    if !target.is_specified() {
        anyhow::bail!(
            "no target — pass --all, --groups <a,b>, or --pcs <pc1,pc2> (or use a Schedule for waves)"
        );
    }
    let plan = FanoutPlan {
        target,
        rollout: None,
        jitter: args.jitter,
    };

    info!(job_id = %args.job_id, "executing");
    let url = format!(
        "{}/api/exec/{}",
        backend_url.trim_end_matches('/'),
        args.job_id,
    );
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .json(&plan)
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
