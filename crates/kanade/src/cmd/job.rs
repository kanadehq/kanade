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
    /// v0.27: also writes `script_status.{id} = REVOKED` so any
    /// in-flight Command for this manifest gets skipped by the agent's
    /// Layer 2 check (SPEC §2.6.4 (b)). Operator-side: re-create with
    /// `kanade job create <yaml>` + `kanade unrevoke <id>` to undo.
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
    let raw = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // Parse client-side so a malformed YAML errors before any HTTP
    // round-trip — keeps the original error site obvious in operator
    // shells.
    let mut job: Manifest =
        serde_yaml::from_str(&raw).with_context(|| format!("parse {yaml:?}"))?;

    // SPEC §2.4.1: exactly-one-of script / script_file / script_object.
    // Validate BEFORE inlining script_file (Gemini #215 HIGH) so a
    // manifest declaring both `script:` and `script_file:` is caught
    // — otherwise the inlining below would silently merge the two
    // sources into one populated `script`, sneaking the manifest
    // past `Manifest::validate()`'s exclusivity check.
    if let Err(e) = job.validate() {
        anyhow::bail!("{yaml:?}: {e}");
    }

    // SPEC §2.4.1 / #210: `script_file:` is operator-side sugar that
    // points at a repo-local file the CLI inlines into `execute.script`
    // before submission. The backend never sees the field — it works
    // entirely on `script` / `script_object`. Resolution happens here
    // so:
    //   - the operator's failure site for a missing file is the CLI
    //     (where the path is meaningful), not a 400 from a backend
    //     that doesn't share their filesystem;
    //   - the manifest stored in BUCKET_JOBS is the fully-resolved
    //     form — schedules + agents read it as-is.
    // Paths resolve relative to the YAML's parent directory so
    // `scripts/cleanup.ps1` works out of the box for the common
    // `jobs/<name>.yaml` + `jobs/scripts/<name>.ps1` layout.
    let (body, sent_raw) = if let Some(path) = job.execute.script_file.as_deref() {
        let file_path = resolve_script_file_path(yaml, path);
        let script_body = std::fs::read_to_string(&file_path).with_context(|| {
            format!(
                "read script_file {} (referenced from {yaml:?})",
                file_path.display(),
            )
        })?;
        info!(
            script_file = %file_path.display(),
            size = script_body.len(),
            "inlined script_file into execute.script",
        );
        job.execute.script = Some(script_body);
        job.execute.script_file = None;
        // Re-serialize so the backend stores the resolved form.
        // Loses any operator comments / formatting of the original
        // YAML's `execute:` block, but `script_file:` manifests put
        // the interesting content in the separate script anyway.
        let serialized = serde_yaml::to_string(&job)
            .context("re-serialize manifest after script_file inlining")?;
        (serialized, false)
    } else {
        // Inline-script manifests: send the raw body so the backend
        // mirrors it verbatim into BUCKET_JOBS_YAML (preserves comments
        // + block-scalar script indent across SPA edits). Pre-v0.31
        // backends only understood JSON content-type, but
        // `application/yaml` is parsed identically on v0.31+, so the
        // CLI sends YAML unconditionally.
        (raw, true)
    };

    info!(
        job_id = %job.id,
        version = %job.version,
        sent_raw_yaml = sent_raw,
        "upserting job",
    );

    let url = format!("{base}/api/jobs");
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

/// `script_file:` paths are resolved relative to the YAML's parent
/// directory so `jobs/cleanup.yaml` referencing `scripts/cleanup.ps1`
/// finds `jobs/scripts/cleanup.ps1`. Absolute paths pass through
/// unchanged (lets operators point at a shared template tree
/// outside the manifest folder).
fn resolve_script_file_path(yaml: &std::path::Path, script_file: &str) -> PathBuf {
    let p = PathBuf::from(script_file);
    if p.is_absolute() {
        return p;
    }
    match yaml.parent() {
        Some(parent) => parent.join(p),
        None => p,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_script_file_resolves_under_yaml_parent() {
        let yaml = std::path::Path::new("/repo/jobs/cleanup.yaml");
        assert_eq!(
            resolve_script_file_path(yaml, "scripts/cleanup.ps1"),
            std::path::PathBuf::from("/repo/jobs/scripts/cleanup.ps1"),
        );
    }

    #[test]
    fn absolute_script_file_passes_through_unchanged() {
        let yaml = std::path::Path::new("/repo/jobs/cleanup.yaml");
        // Use the platform's absolute-path shape so the assertion is
        // valid on both Unix (`/shared/...`) and Windows (`C:\...`).
        let abs = if cfg!(windows) {
            "C:/shared/templates/cleanup.ps1"
        } else {
            "/shared/templates/cleanup.ps1"
        };
        assert_eq!(
            resolve_script_file_path(yaml, abs),
            std::path::PathBuf::from(abs),
        );
    }

    #[test]
    fn manifest_with_both_script_and_script_file_fails_validation() {
        // Gemini #215 HIGH regression guard: the create flow must
        // call `Manifest::validate()` BEFORE inlining script_file
        // into script, otherwise an operator manifest declaring
        // both sources is silently merged and the duplicate goes
        // undetected. This test exercises the Manifest validator
        // directly — the create() function's own ordering is
        // documented at the call site and covered by integration.
        let yaml = r#"
id: ambiguous
version: 1.0.0
execute:
  shell: powershell
  script: "echo inline"
  script_file: scripts/cleanup.ps1
  timeout: 30s
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse");
        let err = m.validate().expect_err("validate should reject");
        assert!(
            err.contains("only one of"),
            "expected exclusivity error, got: {err}",
        );
    }

    #[test]
    fn bare_yaml_filename_keeps_script_file_relative_to_cwd() {
        // `Path::parent()` returns `Some("")` for a bare filename;
        // joining that with the script_file path is a no-op, so a
        // CLI invocation in the manifest's directory (`kanade job
        // create manifest.yaml`) resolves `script.ps1` against the
        // operator's cwd — which IS the manifest's dir. Same
        // intuitive behavior as the `jobs/cleanup.yaml` case, just
        // without the `jobs/` prefix.
        let yaml = std::path::Path::new("manifest.yaml");
        assert_eq!(
            resolve_script_file_path(yaml, "script.ps1"),
            std::path::PathBuf::from("script.ps1"),
        );
    }
}
