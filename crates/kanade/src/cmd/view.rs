//! `kanade view` — manage [`View`] resources (#743): standalone
//! declarative dashboards over `obs_events` for the Analytics page. Same
//! REST shape as `kanade job` / `kanade schedule` (HTTP to the backend);
//! a view has no `execute` and no schedule, so this is just
//! create / list / export / delete.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::{View, is_valid_resource_id};
use tracing::{info, warn};

use crate::cmd::provenance::{append_origin_yaml, detect_repo_origin, has_top_level_origin};

#[derive(Args, Debug)]
pub struct ViewArgs {
    #[command(subcommand)]
    pub sub: ViewSub,
}

#[derive(Subcommand, Debug)]
pub enum ViewSub {
    /// Upsert one or more views from YAML files.
    ///
    /// Accepts multiple files, a directory (its top-level `*.yaml` /
    /// `*.yml`), and/or glob patterns — e.g. `kanade view create
    /// configs/views/*.yaml`. Each file is registered independently
    /// (fail-soft per file); the command exits non-zero if any fails.
    Create {
        /// View YAML paths (`id` / `widgets`).
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Export registered view YAML (the comment-preserving mirror).
    ///
    /// `kanade view export <id>` prints to stdout; with `--out-dir` it
    /// writes `<dir>/<id>.yaml`. `--all --out-dir <dir>` dumps every
    /// registered view. Round-trips with `create`.
    Export {
        /// View id to export. Omit only with `--all`.
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Export every registered view (requires `--out-dir`).
        #[arg(long, conflicts_with = "id", requires = "out_dir")]
        all: bool,
        /// Directory to write `<id>.yaml` into.
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// List all views currently stored in the views KV.
    List,
    /// Delete a view by its id.
    Delete { id: String },
}

pub async fn execute(backend_url: &str, args: ViewArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        ViewSub::Create { paths } => create_all(base, paths).await,
        ViewSub::Export { id, all, out_dir } => {
            crate::cmd::bulk::export(base, "views", id, all, out_dir).await
        }
        ViewSub::List => list(base).await,
        ViewSub::Delete { id } => delete(base, &id).await,
    }
}

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
        anyhow::bail!("{failures}/{} view manifest(s) failed", files.len());
    }
    Ok(())
}

async fn create_one(base: &str, yaml: &std::path::Path) -> Result<()> {
    // `mut` because the provenance step below appends an `origin:` block.
    let mut body = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // Parse + validate client-side first so a malformed view errors at the
    // operator's shell rather than as the backend's 400; then ship the raw
    // YAML so the backend's BUCKET_VIEWS_YAML mirror keeps comments. #492:
    // strict parse — unknown keys are operator typos at this boundary.
    let view: View = kanade_shared::strict::from_yaml_str(&body)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;
    view.validate()
        .map_err(|e| anyhow::anyhow!("invalid view {yaml:?}: {e}"))?;
    info!(view_id = %view.id, widgets = view.widgets.len(), "upserting view");

    // #678 GitOps provenance — parity with job/schedule create. A view
    // carries no script, so the script_file arg is always `None`.
    if let Some(origin) = detect_repo_origin(yaml, None) {
        if has_top_level_origin(&body) {
            warn!(
                view_id = %view.id,
                "origin: already present in source YAML; preserving it. \
                 If the repo / remote changed, delete + recreate the view \
                 to refresh provenance",
            );
        } else {
            append_origin_yaml(&mut body, &origin).context("append origin provenance")?;
        }
    }

    let url = format!("{base}/api/views");
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
    let payload: serde_json::Value = resp
        .json()
        .await
        .context("parse JSON response from server")?;
    let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let n = payload
        .get("widget_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("✓ {} → view '{id}' ({n} widget(s))", yaml.display());
    Ok(())
}

async fn list(base: &str) -> Result<()> {
    let url = format!("{base}/api/views");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("list failed: {status} — {body}");
    }
    let payload: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

async fn delete(base: &str, id: &str) -> Result<()> {
    // Guard the id before it lands in the URL path — same charset the
    // backend enforces on create, so a stray `/` or `..` fails fast with a
    // clear message rather than silently hitting a normalized URL.
    if !is_valid_resource_id(id) {
        anyhow::bail!("invalid view id '{id}' (allowed: [A-Za-z0-9._-])");
    }
    let url = format!("{base}/api/views/{id}");
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
