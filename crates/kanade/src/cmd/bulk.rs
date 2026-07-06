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

/// Split a raw YAML stream into one raw string per document, cutting on
/// column-0 `---` document separators (#654 follow-up: multi-doc bundle
/// support so `export --all` and `create` round-trip through a single
/// file).
///
/// Why a line splitter and not the parser: `create` ships the operator's
/// *raw* YAML to the backend so the comment-preserving mirror keeps
/// comments + block-scalar indentation. serde_yaml's multi-doc iterator
/// hands back parsed values with no way to recover each document's source
/// span, so it can't preserve raw text. A column-0 line splitter can — and
/// it stays parser-consistent because these manifests are top-level
/// mappings: every block-scalar body (an embedded `script:`) is nested and
/// therefore indented, so a `---` at column 0 is never scalar content, it
/// is always a document boundary (a less-indented line already terminates
/// the scalar for the parser too). A `---` inside a script only appears
/// indented, and stays with its document.
///
/// Comment-/whitespace-only chunks (a header comment above the first
/// `---`, or a trailing separator) are dropped so they don't surface as
/// phantom empty documents. A source with no separator at all is returned
/// verbatim as a single element — byte-exact, so the overwhelmingly common
/// single-document file keeps its exact bytes (CRLF, comments) on the wire.
pub fn split_yaml_documents(text: &str) -> Vec<String> {
    // Fast path: no column-0 `---` marker ⇒ a single document. Return it
    // verbatim so single-doc `create` stays byte-for-byte what it was
    // before multi-doc support (the raw-mirror + provenance code downstream
    // depends on unmodified bytes).
    if !has_doc_separator(text) {
        return if has_yaml_content(text) {
            vec![text.to_string()]
        } else {
            Vec::new()
        };
    }
    let mut docs = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if is_doc_separator(line) {
            docs.push(std::mem::take(&mut cur));
        } else {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    docs.push(cur);
    let filtered: Vec<String> = docs.into_iter().filter(|d| has_yaml_content(d)).collect();
    // At most one content document means this was really a single manifest
    // that merely carried a leading and/or trailing `---` marker. Return the
    // original text verbatim (not the rebuilt chunk, which drops the marker
    // and normalises CRLF→LF) so a single-doc file stays byte-exact for the
    // comment-preserving backend mirror — the same guarantee the no-separator
    // fast path above gives.
    if filtered.len() <= 1 {
        return if has_yaml_content(text) {
            vec![text.to_string()]
        } else {
            Vec::new()
        };
    }
    filtered
}

/// A YAML document-start marker: `---` at column 0 (no leading indent),
/// followed by either end-of-line, only spaces/tabs, or whitespace then a
/// `#` comment (`--- # next job` is a readable separator operators
/// hand-write in a bundle). Indentation would make it block-scalar content,
/// not a separator. Inline *node* content after the marker (e.g.
/// `--- id: b`) is deliberately NOT treated as a separator: these manifests
/// never use it, and cutting there would split a real document mid-node.
fn is_doc_separator(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("---") else {
        return false;
    };
    match rest.chars().next() {
        // Bare `---`.
        None => true,
        // `--- ...`: allowed only if the remainder is blank or a `#` comment.
        Some(' ') | Some('\t') => {
            let after = rest.trim_start_matches([' ', '\t']);
            after.is_empty() || after.starts_with('#')
        }
        // `----`, `---x`, … — not a marker.
        _ => false,
    }
}

fn has_doc_separator(text: &str) -> bool {
    text.lines().any(is_doc_separator)
}

/// True when a chunk holds at least one line that isn't blank or a
/// full-line `#` comment — i.e. something a YAML parser would treat as
/// real content rather than parsing to `null`.
fn has_yaml_content(doc: &str) -> bool {
    doc.lines().any(|l| {
        let t = l.trim_start();
        !t.is_empty() && !t.starts_with('#')
    })
}

/// `export <id>` / `export --all --out-dir <dir>` for either catalog.
///
/// - `--all` with `--out-dir`: list every registered id and write each to
///   `<dir>/<id>.yaml`. Fail-soft per id, non-zero exit overall if any fail.
/// - `--all` without `--out-dir`: stream every entry to stdout as one
///   `---`-separated multi-doc YAML bundle — `kanade {kind} export --all >
///   {kind}.yaml` gives a single file that's easy to diff and that
///   `create` can re-ingest (multi-doc aware). Progress/error lines go to
///   stderr so the piped stdout stays a clean bundle.
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
        let ids = list_ids(base, kind).await?;
        if ids.is_empty() {
            // stderr, not stdout: a `> bundle.yaml` redirect must not get a
            // stray human line written into it.
            eprintln!("no {kind} registered");
            return Ok(());
        }
        let Some(dir) = out_dir else {
            return export_all_bundle(base, kind, &ids).await;
        };
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

/// Stream every entry to stdout as a single `---`-separated multi-doc
/// YAML bundle. Fail-soft per id (a fetch failure logs to stderr and is
/// counted) so one bad entry doesn't abort the whole dump; non-zero exit
/// if any failed. The separator is written *between* successfully-fetched
/// documents only, so a failed id in the middle never leaves a dangling
/// `---`.
async fn export_all_bundle(base: &str, kind: &str, ids: &[String]) -> Result<()> {
    let mut failures = 0usize;
    let mut first = true;
    for id in ids {
        match fetch_yaml(base, kind, id).await {
            Ok(yaml) => {
                if !first {
                    println!("---");
                }
                first = false;
                print!("{yaml}");
                // Each mirror already ends in a newline, but guard so the
                // next `---` can't land on the same line as the last value.
                if !yaml.ends_with('\n') {
                    println!();
                }
            }
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
    fn single_document_is_returned_verbatim() {
        // No separator ⇒ byte-exact passthrough (single element), so the
        // common single-doc `create` path is unchanged, CRLF and all.
        let src = "id: a\r\nversion: \"1\"\r\n# trailing comment\r\n";
        assert_eq!(split_yaml_documents(src), vec![src.to_string()]);
    }

    #[test]
    fn single_document_with_leading_separator_is_returned_verbatim() {
        // A lone document that opens with an explicit `---` marker (and/or
        // ends with one) is still a single manifest: return it byte-exact,
        // marker and CRLF included, not the marker-stripped rebuilt chunk.
        let leading = "---\nid: a\nversion: \"1\"\n";
        assert_eq!(split_yaml_documents(leading), vec![leading.to_string()]);
        let both = "---\r\nid: a\r\n---\r\n";
        assert_eq!(split_yaml_documents(both), vec![both.to_string()]);
    }

    #[test]
    fn splits_on_column_zero_separators() {
        let src = "id: a\n---\nid: b\n---\nid: c\n";
        assert_eq!(
            split_yaml_documents(src),
            vec!["id: a\n", "id: b\n", "id: c\n"]
        );
    }

    #[test]
    fn marker_with_trailing_inline_comment_still_splits() {
        // `--- # label` is a readable separator an operator may hand-write in
        // a bundle; a real YAML parser treats it as a document boundary, so
        // it must split here too (not fold both manifests into one blob).
        let src = "id: a\n--- # job-b\nid: b\n";
        assert_eq!(split_yaml_documents(src), vec!["id: a\n", "id: b\n"]);
    }

    #[test]
    fn marker_with_inline_node_content_is_not_a_separator() {
        // `--- id: b` carries an inline node after the marker — unsupported
        // on purpose (these manifests never write it), so it is NOT a split
        // point. The whole thing is one document, returned verbatim.
        let src = "id: a\n--- id: b\n";
        assert_eq!(split_yaml_documents(src), vec![src.to_string()]);
    }

    #[test]
    fn leading_and_trailing_separators_do_not_make_empty_docs() {
        // A leading document-start marker and a trailing separator + header
        // comment must not yield phantom empty documents.
        let src = "# bundle header\n---\nid: a\n---\nid: b\n---\n";
        assert_eq!(split_yaml_documents(src), vec!["id: a\n", "id: b\n"]);
    }

    #[test]
    fn indented_triple_dash_inside_script_is_not_a_separator() {
        // A `---` line *inside* a block scalar is indented, so it stays with
        // its document instead of splitting it — matching how the YAML
        // parser scopes the scalar. Two jobs, not three-plus fragments.
        let src = "\
id: a
execute:
  script: |
    echo start
    ---
    echo end
---
id: b
";
        let docs = split_yaml_documents(src);
        assert_eq!(docs.len(), 2, "got: {docs:?}");
        assert!(
            docs[0].contains("    ---"),
            "indented --- kept: {:?}",
            docs[0]
        );
        assert!(docs[1].starts_with("id: b"));
    }

    #[test]
    fn separator_with_trailing_whitespace_still_splits() {
        let src = "id: a\n---  \nid: b\n";
        assert_eq!(split_yaml_documents(src), vec!["id: a\n", "id: b\n"]);
    }

    #[test]
    fn comment_or_blank_only_source_yields_no_documents() {
        assert!(split_yaml_documents("# just a comment\n\n").is_empty());
        assert!(split_yaml_documents("   \n\t\n").is_empty());
        assert!(split_yaml_documents("").is_empty());
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
