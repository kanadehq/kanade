//! GitOps provenance detection shared by `kanade job create` and
//! `kanade schedule create` (#678 / #695).
//!
//! When a YAML artifact lives inside a Git (or jj) work tree, we stamp
//! it with its repo-relative path so the SPA can render it read-only and
//! point edits back at the repo instead of letting a ClickOps edit
//! silently diverge from Git (SPEC design principle #3: 設定駆動 YAML +
//! Git). The detected [`RepoOrigin`] is appended to the YAML body the
//! CLI submits, and ridealong on the stored manifest / schedule.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kanade_shared::manifest::RepoOrigin;

/// Append a top-level `origin:` block to `yaml`. Serialised via
/// serde_yaml — its single-line scalars are fine (only multi-line
/// strings trip the block-scalar gap), so URL values get quoted
/// correctly.
pub(crate) fn append_origin_yaml(yaml: &mut String, origin: &RepoOrigin) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Wrap<'a> {
        origin: &'a RepoOrigin,
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
/// artifact (a duplicate key is a YAML parse error).
pub(crate) fn has_top_level_origin(yaml: &str) -> bool {
    yaml.lines().any(|l| {
        // `contains(':')` rules out a bare `origin` with no colon (not a
        // valid mapping key, but it would otherwise false-match) — claude
        // review #698.
        !l.starts_with(char::is_whitespace)
            && l.contains(':')
            && l.split(':').next().map(str::trim) == Some("origin")
    })
}

/// Detect GitOps provenance for the YAML at `yaml`. Returns `None` when
/// the file isn't inside a versioned work tree (VCS missing, or a
/// one-off artifact outside any repo) — those stay SPA-editable. Git
/// first (SPEC §3 is literally "Git で管理"), then a `jj` fallback so jj
/// checkouts of this very repo are covered too. The remote URL is
/// best-effort via git only; a jj-only workspace just records the
/// `path` (no repo link). `script_file` is the caller's already-resolved
/// absolute path of a job's inlined `script_file:` (jobs only; schedules
/// pass `None`), recorded relative to the repo root when present.
pub(crate) fn detect_repo_origin(yaml: &Path, script_file: Option<&Path>) -> Option<RepoOrigin> {
    let dir = yaml
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Track which backend resolved the root so we only ask *git* for a
    // remote when there's actually a git backend — in a jj-only tree
    // `git remote` is dead work that always fails.
    let (toplevel, git_backed) = match vcs_output(&dir, "git", &["rev-parse", "--show-toplevel"]) {
        Some(t) => (t, true),
        None => (vcs_output(&dir, "jj", &["root"])?, false),
    };
    let toplevel = PathBuf::from(toplevel.trim());
    let path = repo_relative(&toplevel, yaml)?;
    // Remote is git-only + best-effort, and credentials get stripped: a
    // token-bearing remote (`https://<token>@host/…`) must never land in
    // the stored artifact or the SPA's clickable link.
    let repo = git_backed
        .then(|| vcs_output(&dir, "git", &["remote", "get-url", "origin"]))
        .flatten()
        .and_then(|s| sanitize_repo_remote(&s));
    let script_file = script_file.and_then(|sf| repo_relative(&toplevel, sf));
    Some(RepoOrigin {
        path,
        repo,
        script_file,
    })
}

/// Strip any embedded userinfo (`user:pass@` / `token@`) from an http(s)
/// or ssh remote URL so credentials can't leak into the stored artifact
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
fn vcs_output(dir: &Path, prog: &str, args: &[&str]) -> Option<String> {
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
fn repo_relative(toplevel: &Path, file: &Path) -> Option<String> {
    let top = toplevel.canonicalize().ok()?;
    let abs = file.canonicalize().ok()?;
    let rel = abs.strip_prefix(&top).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_top_level_origin_key() {
        assert!(has_top_level_origin("id: j\norigin:\n  path: x\n"));
        assert!(has_top_level_origin("origin: {}\n"));
        // Indented `origin:` (a nested map key) is NOT top-level.
        assert!(!has_top_level_origin("execute:\n  origin: nope\n"));
        assert!(!has_top_level_origin("id: j\nversion: 1.0.0\n"));
        // A bare `origin` with no colon isn't a mapping key (#698 review).
        assert!(!has_top_level_origin("origin\n"));
        assert!(!has_top_level_origin("origin_path: x\n"));
    }

    #[test]
    fn appends_parseable_origin_block() {
        let mut yaml = String::from("id: j\nversion: 1.0.0\n");
        append_origin_yaml(
            &mut yaml,
            &RepoOrigin {
                path: "configs/jobs/j.yaml".into(),
                repo: Some("https://github.com/o/r".into()),
                script_file: None,
            },
        )
        .expect("append");
        assert!(has_top_level_origin(&yaml));
        #[derive(serde::Deserialize)]
        struct Probe {
            origin: RepoOrigin,
        }
        let p: Probe = serde_yaml::from_str(&yaml).expect("parse appended");
        assert_eq!(p.origin.path, "configs/jobs/j.yaml");
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
        assert_eq!(
            sanitize_repo_remote("https://github.com/o/r").as_deref(),
            Some("https://github.com/o/r"),
        );
        assert_eq!(sanitize_repo_remote("   ").as_deref(), None);
    }

    #[test]
    fn detect_repo_origin_resolves_in_repo_checkout() {
        // This crate's Cargo.toml is a stable file under the kanade work
        // tree (git in CI / a normal clone, jj in a colocated dev
        // checkout). Detection must resolve its repo-relative path via
        // whichever VCS is present. In a VCS-less sandbox (e.g. an
        // extracted crate tarball during `cargo publish` verify),
        // detection is correctly `None` and there is nothing to assert.
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        if let Some(origin) = detect_repo_origin(&here, None) {
            assert!(
                origin.path.ends_with("crates/kanade/Cargo.toml"),
                "unexpected repo-relative path: {}",
                origin.path,
            );
        }
    }
}
