//! `kanade job` — manage the job catalog (BUCKET_JOBS).
//!
//! A registered Job is just a [`Manifest`] keyed by its `id`.
//! Schedules and ad-hoc deploys reference it by id; editing a job
//! in-place rewrites what subsequent fires deploy.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::Manifest;

use crate::cmd::provenance::{append_origin_yaml, detect_repo_origin, has_top_level_origin};
use tracing::{info, warn};

#[derive(Args, Debug)]
pub struct JobArgs {
    #[command(subcommand)]
    pub sub: JobSub,
}

#[derive(Subcommand, Debug)]
pub enum JobSub {
    /// Upsert one or more jobs into the catalog from YAML manifests.
    ///
    /// Accepts multiple files, a directory (its top-level `*.yaml` /
    /// `*.yml`), and/or glob patterns — e.g. `kanade job create
    /// configs/jobs/*.yaml` or `kanade job create configs/jobs/`. Each
    /// file is registered independently (fail-soft per file); the
    /// command exits non-zero if any file fails.
    Create {
        /// Job YAML paths (Manifest body — `id` / `version` / `target` /
        /// `execute` / optional `inventory`). Globs / directories are
        /// expanded CLI-side, so quote a glob to keep your shell from
        /// expanding it first.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Validate one or more job manifests WITHOUT submitting them.
    ///
    /// Runs the exact client-side checks `create` does — strict YAML
    /// parse (unknown keys rejected with their paths, #492),
    /// `Manifest::validate()` semantic checks (SPEC §2.4.1 exclusivity,
    /// shell limits, finalize/check/collect blocks), and a `script_file:`
    /// existence check — but never contacts the backend. Built for CI /
    /// pre-commit hooks: `kanade job validate configs/jobs/*.yaml`.
    /// Accepts files, directories, and globs like `create`; exits
    /// non-zero if any file fails so it gates a pipeline.
    Validate {
        /// Job YAML paths to check. Globs / directories are expanded
        /// CLI-side, so quote a glob to keep your shell from expanding it
        /// first.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Export registered job YAML (the comment-preserving mirror).
    ///
    /// `kanade job export <id>` prints to stdout; with `--out-dir` it
    /// writes `<dir>/<id>.yaml`. `--all --out-dir <dir>` dumps every
    /// registered job as one file per id; `--all` *without* `--out-dir`
    /// streams them all to stdout as a single `---`-separated bundle
    /// (`kanade job export --all > jobs.yaml`) — easy to diff against the
    /// repo and re-appliable with `create`. Round-trips with `create`:
    /// `kanade job export foo > foo.yaml` → edit → `kanade job create
    /// foo.yaml`.
    Export {
        /// Job id to export. Omit only with `--all`.
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Export every registered job. With `--out-dir`, one file per id;
        /// without it, a `---`-separated bundle on stdout.
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// Directory to write `<id>.yaml` into. With `--all`, omit it to
        /// stream a bundle to stdout; for a single id, omit it to print to
        /// stdout instead.
        #[arg(long)]
        out_dir: Option<PathBuf>,
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
        JobSub::Create { paths } => create_all(base, paths).await,
        // Offline check — no backend round-trip, so `base` is unused here.
        JobSub::Validate { paths } => validate_all(paths),
        JobSub::Export { id, all, out_dir } => {
            crate::cmd::bulk::export(base, "jobs", id, all, out_dir).await
        }
        JobSub::List => list(base).await,
        JobSub::Delete { id } => delete(base, &id).await,
    }
}

/// Expand the operator's `create` arguments (files / dirs / globs) and
/// upsert each manifest. Fail-soft per file so one bad manifest in a
/// batch doesn't abort the rest; the command still exits non-zero if any
/// file failed (#654).
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
        anyhow::bail!("{failures}/{} job manifest(s) failed", files.len());
    }
    Ok(())
}

async fn create_one(base: &str, yaml: &std::path::Path) -> Result<()> {
    let raw = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    // A file may bundle several manifests as a `---`-separated multi-doc
    // stream (e.g. `kanade job export --all > jobs.yaml`); apply each one
    // independently. A single-document file takes the fast path so its raw
    // bytes and error messages are byte-for-byte what they were before
    // multi-doc support.
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

/// Parse + submit one manifest document — one entry of a possibly
/// multi-document file. `raw` is that document's exact source text, kept
/// verbatim so the backend's comment-preserving YAML mirror stays faithful.
async fn create_one_doc(base: &str, yaml: &std::path::Path, raw: &str) -> Result<()> {
    // Parse client-side so a malformed YAML errors before any HTTP
    // round-trip — keeps the original error site obvious in operator
    // shells. #492: strict parse — unknown keys are typos at this
    // boundary and are rejected with their paths (the deny_unknown_
    // fields attribute moved off the types so fleet reads stay
    // tolerant).
    let mut job: Manifest = kanade_shared::strict::from_yaml_str(raw)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;

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
    // #678: GitOps provenance. When the source YAML lives inside a Git
    // work tree, stamp the manifest with its repo-relative path (+ the
    // remote URL and the script_file it inlines) so the SPA renders the
    // job read-only and points edits back at the repo instead of letting
    // a ClickOps edit silently diverge from Git (SPEC design principle
    // #3). Detected here, before inlining nulls `script_file`. `None`
    // (not in a repo / git absent) ⇒ the job stays SPA-editable. We
    // resolve the script_file to its absolute path up front so the
    // shared detector can record it repo-relative (#695 extracted the
    // detection into `cmd::provenance` for schedules to reuse).
    let script_file_abs = job
        .execute
        .script_file
        .as_deref()
        .map(|p| resolve_script_file_path(yaml, p));
    let origin = detect_repo_origin(yaml, script_file_abs.as_deref());

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
        job.origin = origin;
        // Re-serialize so the backend stores the resolved form. Comments
        // / formatting of the original `execute:` block are still lost
        // (the script_file path never carried them), but #678: the script
        // body is emitted as a literal block scalar instead of
        // serde_yaml's `\n`-escaped double-quoted blob — so the YAML the
        // SPA mirrors back is the clean `.ps1`, not a garbled wall.
        let serialized = manifest_to_block_scalar_yaml(&job)
            .context("re-serialize manifest after script_file inlining")?;
        (serialized, false)
    } else {
        // Inline-script manifests: send the raw body so the backend
        // mirrors it verbatim into BUCKET_JOBS_YAML (preserves comments
        // + block-scalar script indent across SPA edits). Pre-v0.31
        // backends only understood JSON content-type, but
        // `application/yaml` is parsed identically on v0.31+, so the
        // CLI sends YAML unconditionally.
        let mut out = raw.to_string();
        // #678: append GitOps provenance without disturbing the
        // operator's formatting. If the YAML already declares a top-level
        // `origin:` (operator hand-wrote one, or this is a previously
        // CLI-stamped file re-applied), preserve it — appending would be
        // a duplicate-key parse error — but warn, since a changed
        // repo/remote won't be reflected without a delete + recreate
        // (claude review).
        if let Some(o) = &origin {
            if has_top_level_origin(&out) {
                warn!(
                    job_id = %job.id,
                    "origin: already present in source YAML; preserving it. \
                     If the repo / remote changed, delete + recreate the job \
                     to refresh provenance",
                );
            } else {
                append_origin_yaml(&mut out, o).context("append origin provenance")?;
            }
        }
        (out, origin.is_none())
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
    // Concise per-file line so a bulk `create` reads as a digestible
    // list rather than N pretty-printed JSON blobs (#654). The summary
    // payload is `{id, version, ...}`; fall back to the source path if a
    // field is somehow absent.
    let payload: serde_json::Value = resp
        .json()
        .await
        .context("parse JSON response from server")?;
    let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let version = payload
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    println!("✓ {} → job '{id}' v{version}", yaml.display());
    Ok(())
}

/// Expand the operator's `validate` arguments (files / dirs / globs) and
/// check each manifest without submitting it. Fail-soft per file so one
/// bad manifest in a batch doesn't hide the rest; exits non-zero if any
/// file failed so the command gates CI.
fn validate_all(paths: Vec<PathBuf>) -> Result<()> {
    let files = crate::cmd::bulk::expand_manifest_paths(&paths)?;
    let mut failures = 0usize;
    for f in &files {
        if let Err(e) = validate_one(f) {
            eprintln!("✗ {}: {e:#}", f.display());
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!(
            "{failures}/{} job manifest(s) failed validation",
            files.len()
        );
    }
    Ok(())
}

/// Parse + validate one job manifest WITHOUT submitting it. Mirrors the
/// client-side checks `create_one` runs up to (but not including) the
/// HTTP POST: strict parse → `Manifest::validate()` → `script_file:`
/// resolves to a real file. No GitOps provenance / inlining happens —
/// validation must not mutate the operator's tree.
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

/// Parse + validate one manifest document (one entry of a possibly
/// multi-document file) without submitting it.
fn validate_one_doc(yaml: &std::path::Path, raw: &str) -> Result<()> {
    // #492: strict parse so unknown keys (operator typos) are rejected
    // with their paths, exactly as `create` would reject them.
    let job: Manifest = kanade_shared::strict::from_yaml_str(raw)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;
    // SPEC §2.4.1 exclusivity + the rest of the semantic checks.
    if let Err(e) = job.validate() {
        anyhow::bail!("{yaml:?}: {e}");
    }
    // `script_file:` is operator-side sugar the CLI inlines at create
    // time (#210); a missing file is a create-time failure, so surface
    // it here too rather than letting validate pass on a manifest that
    // `create` would reject. `is_file()` (not `exists()`): an empty path
    // or a directory satisfies `exists()` but `create`'s
    // `read_to_string` would still choke on it — so green-lighting it
    // here would diverge from what `create` accepts (gemini / CodeRabbit).
    if let Some(path) = job.execute.script_file.as_deref() {
        let file_path = resolve_script_file_path(yaml, path);
        if !file_path.is_file() {
            anyhow::bail!(
                "{yaml:?}: script_file {} not found or is not a file",
                file_path.display(),
            );
        }
    }
    println!(
        "✓ {} → job '{}' v{} (valid)",
        yaml.display(),
        job.id,
        job.version,
    );
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

/// Render `job` to YAML with `execute.script` as a literal block
/// scalar (#678). `serde_yaml` 0.9 can't emit block scalars, so a
/// direct `to_string` turns a multi-line script into one
/// double-quoted `\n`-escaped line — the "garbled blob" operators saw
/// in the SPA editor. We serialise with a single-line sentinel in the
/// script slot, then splice the real body back in as a `|` block
/// indented under the sentinel line's `script:` key. Everything else
/// (field order, quoting, the appended `origin:`) is left to
/// serde_yaml, which only mishandles multi-line strings.
fn manifest_to_block_scalar_yaml(job: &Manifest) -> Result<String> {
    const SENTINEL: &str = "__KANADE_SCRIPT_BLOCK_SENTINEL__";
    // Normalise CRLF → LF up front. YAML block scalars normalise line
    // breaks to `\n` on parse anyway (so a CRLF script_file round-trips
    // as LF regardless), and PowerShell runs LF scripts fine — doing it
    // here keeps stray `\r` bytes out of the YAML mirror the SPA shows.
    let body = job
        .execute
        .script
        .clone()
        .unwrap_or_default()
        .replace("\r\n", "\n");
    let mut stub = job.clone();
    stub.execute.script = Some(SENTINEL.to_string());
    let serialized = serde_yaml::to_string(&stub).context("serialize manifest")?;

    let mut out = String::with_capacity(serialized.len() + body.len());
    let mut spliced = false;
    for line in serialized.lines() {
        if !spliced && line.contains(SENTINEL) {
            let indent: String = line.chars().take_while(|c| *c == ' ').collect();
            // Strip a single trailing newline so `|` (clip) re-adds
            // exactly one; fall back to `|-` (strip) when the body had
            // none, so we don't fabricate a trailing newline.
            let (chomp, content) = match body.strip_suffix('\n') {
                Some(stripped) => ("|", stripped),
                None => ("|-", body.as_str()),
            };
            out.push_str(&indent);
            out.push_str("script: ");
            out.push_str(chomp);
            out.push('\n');
            let body_indent = format!("{indent}  ");
            for bl in content.split('\n') {
                if bl.is_empty() {
                    // Blank lines need no indentation in a block scalar.
                    out.push('\n');
                } else {
                    out.push_str(&body_indent);
                    out.push_str(bl);
                    out.push('\n');
                }
            }
            spliced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !spliced {
        anyhow::bail!("script sentinel vanished during YAML serialization");
    }
    Ok(out)
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

    fn inline_manifest() -> Manifest {
        serde_yaml::from_str(
            "id: j\nversion: 1.0.0\nexecute:\n  shell: powershell\n  script: \"x\"\n  timeout: 30s\n",
        )
        .expect("parse base manifest")
    }

    #[test]
    fn block_scalar_yaml_roundtrips_multiline_script() {
        // #678: a script_file-inlined manifest must serialise the body
        // as a literal block scalar, NOT serde_yaml's `\n`-escaped
        // double-quoted blob (that blob is what made the SPA editor
        // unreadable). And it must round-trip back to the exact body.
        let mut m = inline_manifest();
        let script = "#requires -Version 5.1\nWrite-Output 'hi'\n\nGet-Date\n";
        m.execute.script = Some(script.to_string());
        m.origin = Some(kanade_shared::manifest::RepoOrigin {
            path: "configs/jobs/j.yaml".into(),
            repo: Some("git@github.com:o/r.git".into()),
            script_file: Some("configs/jobs/scripts/j.ps1".into()),
        });
        let yaml = manifest_to_block_scalar_yaml(&m).expect("serialize");
        assert!(
            yaml.contains("script: |"),
            "expected a block scalar, got:\n{yaml}"
        );
        assert!(
            !yaml.contains("script: \""),
            "script must be a block scalar, not a quoted blob:\n{yaml}"
        );
        let back: Manifest = serde_yaml::from_str(&yaml).expect("re-parse");
        assert_eq!(back.execute.script.as_deref(), Some(script));
        assert_eq!(
            back.origin.as_ref().map(|o| o.path.as_str()),
            Some("configs/jobs/j.yaml"),
        );
        back.validate().expect("spliced manifest still validates");
    }

    #[test]
    fn block_scalar_yaml_preserves_no_trailing_newline() {
        // A body without a trailing newline must use `|-` (strip) so we
        // don't fabricate one on round-trip.
        let mut m = inline_manifest();
        let script = "line1\nline2";
        m.execute.script = Some(script.to_string());
        let yaml = manifest_to_block_scalar_yaml(&m).expect("serialize");
        assert!(
            yaml.contains("script: |-"),
            "expected strip-chomp block, got:\n{yaml}"
        );
        let back: Manifest = serde_yaml::from_str(&yaml).expect("re-parse");
        assert_eq!(back.execute.script.as_deref(), Some(script));
    }

    /// Repo root two levels up from `crates/kanade` — shared by the
    /// real-config tests, which skip in a sourceless sandbox.
    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/kanade has a repo root two levels up")
            .to_path_buf()
    }

    #[test]
    fn validate_one_accepts_a_real_script_file_job() {
        // End-to-end on the real artifact: the installer manifest uses
        // `script_file:`, so this exercises both `Manifest::validate()`
        // and the script_file existence branch.
        let yaml = repo_root().join("configs/jobs/installers/install-kanade-client.yaml");
        if !yaml.exists() {
            return;
        }
        validate_one(&yaml).expect("real script_file job validates offline");
    }

    #[test]
    fn validate_one_rejects_missing_script_file() {
        // A manifest whose `script_file:` points nowhere must fail
        // validate — otherwise `create` would later reject a manifest
        // `validate` had blessed. A unique temp dir (so the relative
        // script path resolves to a definitely-absent file) keeps
        // concurrent test runs from racing on a shared path (claude bot).
        let dir = tempfile::tempdir().expect("mk temp dir");
        let yaml_path = dir.path().join("job.yaml");
        std::fs::write(
            &yaml_path,
            "id: j\nversion: 1.0.0\nexecute:\n  shell: powershell\n  script_file: does-not-exist.ps1\n  timeout: 30s\n",
        )
        .expect("write temp manifest");
        let err = validate_one(&yaml_path).expect_err("missing script_file must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("script_file") && msg.contains("not found"),
            "expected a missing-script_file error, got: {msg}",
        );
    }

    #[test]
    fn validate_one_rejects_unknown_key() {
        // #492 parity: a typo'd top-level key is a strict-parse failure.
        // (Jobs have no flattened field, so unknown top-level keys ARE
        // rejected — unlike `Schedule`, see its test.)
        let dir = tempfile::tempdir().expect("mk temp dir");
        let yaml_path = dir.path().join("job.yaml");
        std::fs::write(
            &yaml_path,
            "id: j\nversion: 1.0.0\nexecute:\n  shell: powershell\n  script: x\n  timeout: 30s\nbogus_key: 1\n",
        )
        .expect("write temp manifest");
        let err = validate_one(&yaml_path).expect_err("unknown key must fail");
        assert!(
            format!("{err:#}").contains("bogus_key"),
            "expected the unknown key in the error, got: {err:#}",
        );
    }

    #[test]
    fn install_kanade_client_yaml_renders_clean() {
        // End-to-end on the real artifact (#678): take the actual
        // `script_file:` manifest + its `.ps1` and run the same inline →
        // block-scalar transform `create()` uses. The output must be a
        // clean literal block (no `\n`-escaped blob) that round-trips —
        // i.e. the YAML the SPA mirrors back is readable, not the garbled
        // wall that motivated this issue. Skips in a sourceless sandbox.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/kanade has a repo root two levels up");
        let yaml_path = root.join("configs/jobs/installers/install-kanade-client.yaml");
        let ps1_path = root.join("configs/jobs/installers/scripts/install-kanade-client.ps1");
        if !yaml_path.exists() || !ps1_path.exists() {
            return;
        }
        let raw = std::fs::read_to_string(&yaml_path).expect("read manifest");
        let mut job: Manifest = kanade_shared::strict::from_yaml_str(&raw).expect("parse manifest");
        let body = std::fs::read_to_string(&ps1_path).expect("read script");
        job.execute.script = Some(body.clone());
        job.execute.script_file = None;
        let out = manifest_to_block_scalar_yaml(&job).expect("serialize");
        assert!(out.contains("script: |"), "expected block scalar:\n{out}");
        // The blob regression is `script: "...\n..."` (a quoted scalar);
        // a literal `\n` can legitimately appear inside the script body
        // (this .ps1 has one in a comment), so check for the quoted form
        // specifically rather than any `\n` substring.
        assert!(
            !out.contains("script: \""),
            "script must be a block scalar, not a quoted blob:\n{out}"
        );
        let back: Manifest = serde_yaml::from_str(&out).expect("re-parse");
        // Block scalars normalise CRLF → LF; this .ps1 is a Windows
        // (CRLF) file, so compare against the LF-normalised body.
        assert_eq!(
            back.execute.script.as_deref(),
            Some(body.replace("\r\n", "\n").as_str()),
        );
        back.validate()
            .expect("round-tripped manifest still validates");
    }
}
