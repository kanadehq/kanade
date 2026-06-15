//! Shared plumbing for bulk `create` (glob / directory expansion) and
//! `export` (#654), used by both the `job` and `schedule` subcommands.
//!
//! The two catalogs expose the same REST shape — `GET /api/{kind}`
//! lists rows that each carry a top-level `id`, and
//! `GET /api/{kind}/{id}/yaml` returns the comment-preserving YAML
//! mirror — so one set of helpers serves both, keyed by `kind`
//! (`"jobs"` / `"schedules"`). The per-manifest `create` logic still
//! lives in each subcommand module (it differs: jobs inline
//! `script_file:` + stamp GitOps provenance, schedules don't); this
//! module only owns the argument-expansion and export halves they share.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Expand operator-supplied `create` arguments — a mix of plain files,
/// directories, and glob patterns — into a deduplicated, sorted list of
/// concrete manifest files.
///
/// - A path containing glob metacharacters (`*`, `?`, `[`) is matched
///   with the `glob` crate, so `configs/jobs/*.yaml` works even when the
///   shell didn't expand it (PowerShell never does). Backslashes are
///   normalised to `/` first so a Windows-style pattern still matches.
/// - A directory contributes its top-level `*.yaml` / `*.yml` files
///   (non-recursive — a manifest tree is flat in practice, and recursing
///   would risk sweeping in unrelated YAML).
/// - Anything else is taken verbatim; existence is checked when the file
///   is read, so an explicit `foo.txt` is honored regardless of suffix.
///
/// A glob / directory that matches nothing is an error (a silent empty
/// result reads as "done, registered everything" when nothing happened).
pub fn expand_manifest_paths(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    // BTreeSet both dedupes (a file can match two patterns) and gives a
    // deterministic order independent of the argument order.
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for input in inputs {
        let s = input.to_string_lossy();
        if s.contains(['*', '?', '[']) {
            let pattern = s.replace('\\', "/");
            let mut matched = 0usize;
            for entry in glob::glob(&pattern).with_context(|| format!("bad glob pattern '{s}'"))? {
                let path = entry.with_context(|| format!("glob '{s}'"))?;
                if path.is_file() {
                    out.insert(path);
                    matched += 1;
                }
            }
            if matched == 0 {
                bail!("glob '{s}' matched no files");
            }
        } else if input.is_dir() {
            let mut found = 0usize;
            for entry in
                std::fs::read_dir(input).with_context(|| format!("read dir {}", input.display()))?
            {
                let path = entry?.path();
                if path.is_file() && has_yaml_ext(&path) {
                    out.insert(path);
                    found += 1;
                }
            }
            if found == 0 {
                bail!("directory {} has no .yaml / .yml files", input.display());
            }
        } else {
            out.insert(input.clone());
        }
    }
    Ok(out.into_iter().collect())
}

fn has_yaml_ext(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("yaml") | Some("yml")
    )
}

/// `export <id>` / `export --all --out-dir <dir>` for either catalog.
///
/// - `--all` (requires `--out-dir`): list every registered id and write
///   each to `<dir>/<id>.yaml`. Fail-soft per id, non-zero exit overall
///   if any fail.
/// - single `id` with `--out-dir`: write `<dir>/<id>.yaml`.
/// - single `id` without `--out-dir`: print the YAML to stdout (the
///   `kanade {kind} export foo > foo.yaml` round-trip with `create`).
pub async fn export(
    base: &str,
    kind: &str,
    id: Option<String>,
    all: bool,
    out_dir: Option<PathBuf>,
) -> Result<()> {
    if all {
        let Some(dir) = out_dir else {
            bail!("--all requires --out-dir <dir>");
        };
        let ids = list_ids(base, kind).await?;
        if ids.is_empty() {
            println!("no {kind} registered");
            return Ok(());
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create out dir {}", dir.display()))?;
        let mut failures = 0usize;
        for id in &ids {
            match fetch_yaml(base, kind, id)
                .await
                .and_then(|y| write_yaml(&dir, id, &y))
            {
                Ok(path) => println!("✓ {id} → {}", path.display()),
                Err(e) => {
                    eprintln!("✗ {id}: {e:#}");
                    failures += 1;
                }
            }
        }
        if failures > 0 {
            bail!("{failures}/{} {kind} failed to export", ids.len());
        }
        Ok(())
    } else {
        let Some(id) = id else {
            bail!("provide an id to export, or --all --out-dir <dir>");
        };
        let yaml = fetch_yaml(base, kind, &id).await?;
        match out_dir {
            Some(dir) => {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("create out dir {}", dir.display()))?;
                let path = write_yaml(&dir, &id, &yaml)?;
                println!("✓ {id} → {}", path.display());
            }
            // No trailing println: the YAML already ends with a newline,
            // so the redirect target is a clean re-creatable file.
            None => print!("{yaml}"),
        }
        Ok(())
    }
}

/// `GET /api/{kind}/{id}/yaml` → the registered (comment-preserving)
/// YAML source for one entry.
async fn fetch_yaml(base: &str, kind: &str, id: &str) -> Result<String> {
    let url = format!("{base}/api/{kind}/{id}/yaml");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("export '{id}' failed: {status} — {body}");
    }
    resp.text()
        .await
        .with_context(|| format!("read body of {url}"))
}

/// List every registered id for the catalog. Rows of `GET /api/{kind}`
/// each carry a top-level `id` (jobs flatten the Manifest, schedules are
/// the Schedule struct), so a shape-agnostic `id` pluck covers both.
async fn list_ids(base: &str, kind: &str) -> Result<Vec<String>> {
    let url = format!("{base}/api/{kind}");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("list {kind} failed: {}", resp.status());
    }
    let rows: Vec<serde_json::Value> = resp
        .json()
        .await
        .with_context(|| format!("parse JSON response from {url}"))?;
    Ok(rows
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect())
}

/// Write `<dir>/<id>.yaml`, returning the path written. Ids are
/// server-registered slugs, but guard against one carrying path
/// separators (escape `dir`), being a lone `.` / `..` (which would write
/// `.yaml` / `..yaml` rather than a real entry), or holding a NUL (which
/// `std::fs::write` would reject anyway, but with an opaque OS error) so
/// a malformed id always fails with a clear message. Separators are
/// rejected first, so the only dot-component cases left are bare `.`/`..`.
fn write_yaml(dir: &Path, id: &str, yaml: &str) -> Result<PathBuf> {
    if id.contains(['/', '\\', '\0']) || id == "." || id == ".." {
        bail!("refusing to write unsafe id (path separators / dot components / NUL): '{id}'");
    }
    let path = dir.join(format!("{id}.yaml"));
    std::fs::write(&path, yaml).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn directory_expands_to_yaml_and_yml_only() {
        let tmp = std::env::temp_dir().join(format!("kanade-bulk-dir-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        write(&tmp, "a.yaml", "id: a");
        write(&tmp, "b.yml", "id: b");
        write(&tmp, "README.md", "nope");
        let got = expand_manifest_paths(std::slice::from_ref(&tmp)).unwrap();
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.yaml", "b.yml"]);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn glob_matches_and_dedupes_against_explicit_path() {
        let tmp = std::env::temp_dir().join(format!("kanade-bulk-glob-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let a = write(&tmp, "one.yaml", "id: one");
        write(&tmp, "two.yaml", "id: two");
        let pattern = PathBuf::from(format!("{}/*.yaml", tmp.display()));
        // Pass the glob AND an explicit overlapping file: the result must
        // dedupe to two distinct files, not three.
        let got = expand_manifest_paths(&[pattern, a]).unwrap();
        assert_eq!(got.len(), 2);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn plain_nonexistent_file_passes_through() {
        // A bare path is taken verbatim — existence is the reader's
        // concern, so the caller can surface a per-file read error.
        let got = expand_manifest_paths(&[PathBuf::from("does-not-exist.yaml")]).unwrap();
        assert_eq!(got, vec![PathBuf::from("does-not-exist.yaml")]);
    }

    #[test]
    fn glob_matching_nothing_errors() {
        let tmp = std::env::temp_dir().join(format!("kanade-bulk-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let pattern = PathBuf::from(format!("{}/*.yaml", tmp.display()));
        assert!(expand_manifest_paths(&[pattern]).is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn directory_with_no_yaml_errors() {
        // A directory that exists but holds no .yaml/.yml must error, not
        // silently expand to nothing (which would read as "registered
        // everything"). The glob-empty case is covered above; this is the
        // sibling directory branch.
        let tmp = std::env::temp_dir().join(format!("kanade-bulk-nodir-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("README.md"), "nope").unwrap();
        assert!(expand_manifest_paths(std::slice::from_ref(&tmp)).is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn write_yaml_rejects_unsafe_ids() {
        let tmp = std::env::temp_dir();
        assert!(write_yaml(&tmp, "../escape", "x").is_err());
        assert!(write_yaml(&tmp, "a/b", "x").is_err());
        assert!(write_yaml(&tmp, "a\\b", "x").is_err());
        assert!(write_yaml(&tmp, ".", "x").is_err());
        assert!(write_yaml(&tmp, "..", "x").is_err());
        assert!(write_yaml(&tmp, "a\0b", "x").is_err());
    }
}
