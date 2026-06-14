//! `kanade job` — manage the job catalog (BUCKET_JOBS).
//!
//! A registered Job is just a [`Manifest`] keyed by its `id`.
//! Schedules and ad-hoc deploys reference it by id; editing a job
//! in-place rewrites what subsequent fires deploy.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::{JobOrigin, Manifest};
use tracing::{info, warn};

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
    // shells. #492: strict parse — unknown keys are typos at this
    // boundary and are rejected with their paths (the deny_unknown_
    // fields attribute moved off the types so fleet reads stay
    // tolerant).
    let mut job: Manifest = kanade_shared::strict::from_yaml_str(&raw)
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
    // (not in a repo / git absent) ⇒ the job stays SPA-editable.
    let origin = detect_repo_origin(yaml, job.execute.script_file.as_deref());

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
        let mut out = raw.clone();
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

/// Append a top-level `origin:` block to `yaml` (#678). Serialised via
/// serde_yaml — its single-line scalars are fine (only multi-line
/// strings trip the block-scalar gap), so URL values get quoted
/// correctly.
fn append_origin_yaml(yaml: &mut String, origin: &JobOrigin) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Wrap<'a> {
        origin: &'a JobOrigin,
    }
    let block = serde_yaml::to_string(&Wrap { origin }).context("serialize origin")?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    yaml.push_str(&block);
    Ok(())
}

/// Does `yaml` already declare a top-level `origin:` key? Cheap guard
/// against appending a duplicate when re-applying an already-stamped
/// manifest (a duplicate key is a YAML parse error).
fn has_top_level_origin(yaml: &str) -> bool {
    yaml.lines().any(|l| {
        !l.starts_with(char::is_whitespace) && l.split(':').next().map(str::trim) == Some("origin")
    })
}

/// Detect GitOps provenance for the manifest at `yaml` (#678). Returns
/// `None` when the file isn't inside a versioned work tree (VCS missing,
/// or a one-off manifest outside any repo) — those jobs stay
/// SPA-editable. Git first (SPEC §3 is literally "Git で管理"), then a
/// `jj` fallback so jj checkouts of this very repo are covered too. The
/// remote URL is best-effort via git only; a jj-only workspace just
/// records the `path` (no repo link). `script_file` (the manifest's
/// raw, pre-resolution reference) is recorded relative to the repo root
/// too, when present.
fn detect_repo_origin(yaml: &std::path::Path, script_file: Option<&str>) -> Option<JobOrigin> {
    let dir = yaml
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Track which backend resolved the root so we only ask *git* for a
    // remote when there's actually a git backend — in a jj-only tree
    // `git remote` is dead work that always fails (claude review).
    let (toplevel, git_backed) = match vcs_output(&dir, "git", &["rev-parse", "--show-toplevel"]) {
        Some(t) => (t, true),
        None => (vcs_output(&dir, "jj", &["root"])?, false),
    };
    let toplevel = PathBuf::from(toplevel.trim());
    let path = repo_relative(&toplevel, yaml)?;
    // Remote is git-only + best-effort, and credentials get stripped: a
    // token-bearing remote (`https://<token>@host/…`) must never land in
    // the stored manifest or the SPA's clickable link (gemini/coderabbit
    // security review).
    let repo = git_backed
        .then(|| vcs_output(&dir, "git", &["remote", "get-url", "origin"]))
        .flatten()
        .and_then(|s| sanitize_repo_remote(&s));
    let script_file = script_file
        .map(|sf| resolve_script_file_path(yaml, sf))
        .and_then(|sf| repo_relative(&toplevel, &sf));
    Some(JobOrigin {
        path,
        repo,
        script_file,
    })
}

/// Strip any embedded userinfo (`user:pass@` / `token@`) from an http(s)
/// or ssh remote URL so credentials can't leak into the stored manifest
/// or the SPA's repo link (#679 review). scp-style remotes
/// (`git@host:owner/repo`) aren't parseable URLs and carry only the
/// non-secret `git` user, so they pass through trimmed; empty → `None`.
fn sanitize_repo_remote(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(mut url) = reqwest::Url::parse(trimmed) {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        return Some(url.to_string());
    }
    Some(trimmed.to_string())
}

/// Run `<prog> <args>` with cwd set to `dir`, returning stdout on a zero
/// exit. `None` on any failure (binary absent, not a repo, non-zero
/// exit) — the caller treats that as "no VCS provenance".
fn vcs_output(dir: &std::path::Path, prog: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(prog)
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Repo-relative, forward-slashed path of `file` under `toplevel`.
/// `None` if either path can't be canonicalised or `file` isn't under
/// `toplevel`.
fn repo_relative(toplevel: &std::path::Path, file: &std::path::Path) -> Option<String> {
    let top = toplevel.canonicalize().ok()?;
    let abs = file.canonicalize().ok()?;
    let rel = abs.strip_prefix(&top).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
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
        m.origin = Some(JobOrigin {
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

    #[test]
    fn detects_top_level_origin_key() {
        assert!(has_top_level_origin("id: j\norigin:\n  path: x\n"));
        assert!(has_top_level_origin("origin: {}\n"));
        // Indented `origin:` (a nested map key) is NOT top-level.
        assert!(!has_top_level_origin("execute:\n  origin: nope\n"));
        assert!(!has_top_level_origin("id: j\nversion: 1.0.0\n"));
    }

    #[test]
    fn detect_repo_origin_resolves_in_repo_checkout() {
        // This crate's Cargo.toml is a stable file under the kanade work
        // tree (git in CI / a normal clone, jj in a colocated dev
        // checkout). Detection must resolve its repo-relative path via
        // whichever VCS is present. In a VCS-less sandbox (e.g. an
        // extracted crate tarball during `cargo publish` verify),
        // detection is correctly `None` and there is nothing to assert.
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        if let Some(origin) = detect_repo_origin(&here, None) {
            assert!(
                origin.path.ends_with("crates/kanade/Cargo.toml"),
                "unexpected repo-relative path: {}",
                origin.path,
            );
        }
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

    #[test]
    fn appends_parseable_origin_block() {
        // The inline-script path keeps the operator's raw YAML and only
        // appends `origin:` — the result must still parse, carry the
        // provenance, and not duplicate an existing key.
        let mut yaml = String::from(
            "id: j\nversion: 1.0.0\nexecute:\n  shell: powershell\n  script: |\n    echo hi\n  timeout: 30s\n",
        );
        append_origin_yaml(
            &mut yaml,
            &JobOrigin {
                path: "configs/jobs/j.yaml".into(),
                repo: Some("https://github.com/o/r".into()),
                script_file: None,
            },
        )
        .expect("append");
        assert!(has_top_level_origin(&yaml));
        let m: Manifest = serde_yaml::from_str(&yaml).expect("parse appended");
        assert_eq!(
            m.origin.expect("origin present").path,
            "configs/jobs/j.yaml"
        );
    }

    #[test]
    fn sanitize_repo_remote_strips_credentials() {
        // Token / password-bearing remotes must not leak into provenance.
        assert_eq!(
            sanitize_repo_remote("https://ghp_secret@github.com/o/r.git").as_deref(),
            Some("https://github.com/o/r.git"),
        );
        assert_eq!(
            sanitize_repo_remote("https://user:pass@example.com/o/r").as_deref(),
            Some("https://example.com/o/r"),
        );
        // scp-style isn't a parseable URL and carries only the non-secret
        // `git` user — passes through trimmed.
        assert_eq!(
            sanitize_repo_remote("git@github.com:o/r.git").as_deref(),
            Some("git@github.com:o/r.git"),
        );
        // Credential-free https is unchanged; blank → None.
        assert_eq!(
            sanitize_repo_remote("https://github.com/o/r").as_deref(),
            Some("https://github.com/o/r"),
        );
        assert_eq!(sanitize_repo_remote("   ").as_deref(), None);
    }
}
