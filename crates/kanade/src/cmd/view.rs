//! `kanade view` — manage [`View`] resources (#743): standalone declarative
//! dashboards (obs_events `aggregate:` widgets and SQL-backed `sql_widgets:`)
//! for the Analytics page. Same REST shape as `kanade job` / `kanade
//! schedule` (HTTP to the backend); a view has no `execute` and no schedule,
//! so this is create / validate / list / export / delete.

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
    /// Validate one or more view manifests WITHOUT submitting them.
    ///
    /// Runs the exact client-side checks `create` does — strict YAML parse
    /// (#492) + `View::validate()` (id charset, per-widget rules for both
    /// `widgets:` and `sql_widgets:`) — but never contacts the backend. One
    /// caveat, shared with `create`: a `sql_widgets[].query` is only checked
    /// read-only at the backend sandbox, so that specific check isn't run
    /// here (structure / placement / render channels ARE). Built for CI /
    /// pre-commit: `kanade view validate configs/views/*.yaml`. Accepts
    /// files, directories, and globs like `create`; exits non-zero if any
    /// file fails.
    Validate {
        /// View YAML paths to check. Globs / directories are expanded
        /// CLI-side, so quote a glob to keep your shell from expanding it.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Export registered view YAML (the comment-preserving mirror).
    ///
    /// `kanade view export <id>` prints to stdout; with `--out-dir` it
    /// writes `<dir>/<id>.yaml`. `--all --out-dir <dir>` dumps every
    /// registered view as one file per id; `--all` *without* `--out-dir`
    /// streams them all to stdout as a single `---`-separated bundle
    /// (`kanade view export --all > views.yaml`) — easy to diff and
    /// re-appliable with `create`. Round-trips with `create`.
    Export {
        /// View id to export. Omit only with `--all`.
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Export every registered view. With `--out-dir`, one file per id;
        /// without it, a `---`-separated bundle on stdout.
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// Directory to write `<id>.yaml` into. With `--all`, omit it to
        /// stream a bundle to stdout; for a single id, omit it to print to
        /// stdout instead.
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
        // Offline check — no backend round-trip, so `base` is unused here.
        ViewSub::Validate { paths } => validate_all(paths),
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

/// Expand the operator's `validate` arguments (files / dirs / globs) and
/// check each view without submitting it. Fail-soft per file so one bad view
/// in a batch doesn't hide the rest; exits non-zero if any file failed so the
/// command gates CI. Mirrors `kanade job/schedule validate`.
fn validate_all(paths: Vec<PathBuf>) -> Result<()> {
    let files = crate::cmd::bulk::expand_manifest_paths(&paths)?;
    let mut failures = 0usize;
    for f in &files {
        // Success lines are printed per-document inside `validate_one_doc`
        // (parity with job/schedule validate and view's own create path), so
        // this loop only surfaces file-level failures.
        if let Err(e) = validate_one(f) {
            eprintln!("✗ {}: {e:#}", f.display());
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!(
            "{failures}/{} view manifest(s) failed validation",
            files.len()
        );
    }
    Ok(())
}

/// Parse + validate one view manifest WITHOUT submitting it. Mirrors the
/// client-side checks `create_one` runs up to (but not including) the HTTP
/// POST: strict parse (#492) → `View::validate()`. No GitOps provenance is
/// appended — validation must not mutate the operator's tree.
fn validate_one(yaml: &std::path::Path) -> Result<()> {
    let raw = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // Validate every document in a possibly multi-doc file (a bundle from
    // `export --all`); single-doc files keep their exact prior behavior.
    let docs = crate::cmd::bulk::split_yaml_documents(&raw);
    match docs.as_slice() {
        [] => anyhow::bail!("{yaml:?}: no YAML documents found"),
        [only] => return validate_one_doc(yaml, only),
        _ => {}
    }
    let mut failures = 0usize;
    for (i, doc) in docs.iter().enumerate() {
        if let Err(e) = validate_one_doc(yaml, doc) {
            eprintln!("✗ {} [doc {}]: {e:#}", yaml.display(), i + 1);
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!(
            "{failures}/{} document(s) in {yaml:?} failed validation",
            docs.len()
        );
    }
    Ok(())
}

/// Parse + validate one view document (one entry of a possibly
/// multi-document file) without submitting it.
fn validate_one_doc(yaml: &std::path::Path, raw: &str) -> Result<()> {
    let view: View = kanade_shared::strict::from_yaml_str(raw)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;
    view.validate()
        .map_err(|e| anyhow::anyhow!("invalid view {yaml:?}: {e}"))?;
    // Per-document success line, matching job/schedule validate and this
    // file's create path, so every document in a bundle is acknowledged
    // (not just the failing ones).
    println!(
        "✓ {} → view '{}' ({} widget(s)) (valid)",
        yaml.display(),
        view.id,
        view.widgets.len(),
    );
    Ok(())
}

async fn create_one(base: &str, yaml: &std::path::Path) -> Result<()> {
    let raw = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // A file may bundle several views as a `---`-separated multi-doc stream
    // (`kanade view export --all > views.yaml`); apply each one
    // independently. A single-document file takes the fast path so its raw
    // bytes and error messages are exactly as before multi-doc support.
    let docs = crate::cmd::bulk::split_yaml_documents(&raw);
    match docs.as_slice() {
        [] => anyhow::bail!("{yaml:?}: no YAML documents found"),
        [only] => return create_one_doc(base, yaml, only).await,
        _ => {}
    }
    let mut failures = 0usize;
    for (i, doc) in docs.iter().enumerate() {
        if let Err(e) = create_one_doc(base, yaml, doc).await {
            eprintln!("✗ {} [doc {}]: {e:#}", yaml.display(), i + 1);
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures}/{} document(s) in {yaml:?} failed", docs.len());
    }
    Ok(())
}

/// Parse + submit one view document — one entry of a possibly
/// multi-document file. `raw` is that document's exact source text, shipped
/// verbatim so the backend's comment-preserving mirror stays faithful.
async fn create_one_doc(base: &str, yaml: &std::path::Path, raw: &str) -> Result<()> {
    // `mut` because the provenance step below appends an `origin:` block.
    let mut body = raw.to_string();
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
