//! #219 file-collection support. A job carrying a `collect:` manifest
//! hint runs its script (which does the gathering work and prints a JSON
//! object listing absolute file paths on stdout); this module then zips
//! those files and uploads the archive to the `OBJECT_COLLECTIONS`
//! Object Store bucket, returning the key the backend records in
//! [`ExecResult::collect_object`](kanade_shared::wire::ExecResult).
//!
//! Wired into BOTH agent run entry points — the NATS-driven
//! `commands::handle_command` and the KLP-driven `klp::handlers::jobs::
//! run_job` (the Client-App path) — via [`maybe_collect`], so a
//! `collect:` + `client:` job fired from the Client App is bundled too.
//!
//! v0 contract: the script lists ALREADY-EXPANDED absolute paths under
//! the hint's `files_field` (default `files`). There is no agent-provided
//! staging dir env var yet (the user-session spawn path builds its own
//! Windows environment block, so injecting one cleanly is deferred); the
//! script writes wherever it likes and lists what to bundle.

use std::io::Write;

use anyhow::{Context, Result, bail};
use kanade_shared::kv::OBJECT_COLLECTIONS;
use kanade_shared::manifest::CollectHint;
use kanade_shared::wire::Command;
use tracing::{info, warn};

/// If `cmd` carries a `collect:` hint, build + upload the bundle and
/// return its Object Store key plus the input paths actually bundled.
/// Best-effort: any failure is logged and returns `None` so the run's
/// `ExecResult` still publishes (a missing bundle must never wedge the
/// result pipeline). Caller should only invoke this on a clean
/// (`exit_code == 0`) run.
///
/// The returned file list lets the finalize hook act on exactly the
/// uploaded files (e.g. delete only what was bundled). On `None` (no
/// hint, or a failed upload) the caller must treat nothing as uploaded.
pub async fn maybe_collect(
    js: &async_nats::jetstream::Context,
    cmd: &Command,
    pc_id: &str,
    stdout: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(String, Vec<String>)> {
    let hint = cmd.collect.as_ref()?;
    match collect_and_upload(js, cmd, hint, pc_id, stdout, now).await {
        Ok((key, files)) => {
            info!(job = %cmd.id, %pc_id, key, files = files.len(), "collect: bundle uploaded");
            Some((key, files))
        }
        Err(e) => {
            warn!(error = %e, job = %cmd.id, %pc_id, "collect: bundle failed; publishing result without a bundle");
            None
        }
    }
}

/// Extract the file-path list from the script's stdout JSON object. The
/// list lives under `hint.files_field` (default `files`) and must be an
/// array of non-empty strings.
fn parse_file_list(stdout: &str, files_field: &str) -> Result<Vec<String>> {
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).context("collect: stdout is not valid JSON")?;
    let arr = value
        .get(files_field)
        .with_context(|| format!("collect: stdout JSON has no '{files_field}' field"))?
        .as_array()
        .with_context(|| format!("collect: '{files_field}' is not an array"))?;
    let mut files = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .context("collect: file-list entry is not a string")?;
        if !s.trim().is_empty() {
            files.push(s.to_string());
        }
    }
    Ok(files)
}

/// Read the listed files and pack them into an in-memory zip, enforcing
/// the cumulative `max_bytes` cap. Missing/unreadable files are skipped
/// with a warning (a partial bundle is more useful than none); the cap
/// counts bytes actually added. Errors only on "no readable files" or a
/// genuine zip-writer failure. Pure/blocking — run under spawn_blocking.
///
/// Returns the zip bytes plus the list of input paths actually packed
/// (skipped files excluded) so the finalize hook can act on exactly what
/// was bundled — e.g. delete only the files that made it into the upload.
fn build_zip(files: Vec<String>, max_bytes: u64) -> Result<(Vec<u8>, Vec<String>)> {
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::<u8>::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut total: u64 = 0;
    let mut added = 0usize;
    let mut added_paths: Vec<String> = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for path in &files {
        // Stat BEFORE reading: (1) skip non-regular-file paths (a dir /
        // named pipe / device would block or misbehave on read), and
        // (2) check the size against the cap so a multi-GB file listed by
        // a buggy script can't OOM the agent before the post-read check
        // fires (gemini).
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, path, "collect: skipping file (metadata failed)");
                continue;
            }
        };
        if !meta.is_file() {
            warn!(path, "collect: skipping non-regular-file path");
            continue;
        }
        if total.saturating_add(meta.len()) > max_bytes {
            bail!(
                "collect: bundle exceeds max_size ({total} + {} > {max_bytes} bytes) at file '{path}'",
                meta.len()
            );
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, path, "collect: skipping unreadable file");
                continue;
            }
        };
        total = total.saturating_add(bytes.len() as u64);
        // Entry name = the file's basename, de-duplicated so two inputs
        // with the same name (e.g. two `config.json`s) don't collide.
        let base = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("file{added}"));
        let mut name = base.clone();
        let mut n = 1;
        while !used_names.insert(name.clone()) {
            name = format!("{n}_{base}");
            n += 1;
        }
        zw.start_file(&name, opts)
            .with_context(|| format!("collect: zip start_file {name}"))?;
        zw.write_all(&bytes)
            .with_context(|| format!("collect: zip write {name}"))?;
        added += 1;
        added_paths.push(path.clone());
    }

    if added == 0 {
        bail!("collect: no readable files to bundle");
    }
    let cursor = zw.finish().context("collect: zip finish")?;
    Ok((cursor.into_inner(), added_paths))
}

async fn collect_and_upload(
    js: &async_nats::jetstream::Context,
    cmd: &Command,
    hint: &CollectHint,
    pc_id: &str,
    stdout: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(String, Vec<String>)> {
    // #821: read collect's own fenced block so a job can also carry a
    // user message and/or other hints' blocks on the same stdout. No
    // fence ⇒ the whole stdout (back-compat for a collect-only job).
    let payload = kanade_shared::manifest::fenced_payload(
        stdout,
        kanade_shared::manifest::COLLECT_BLOCK_BEGIN,
        kanade_shared::manifest::COLLECT_BLOCK_END,
    );
    let files = parse_file_list(payload, &hint.files_field)?;
    if files.is_empty() {
        bail!("collect: '{}' file list is empty", hint.files_field);
    }
    let max_bytes = hint.max_size_bytes();

    // File reads + zip compression are blocking; keep them off the async
    // worker threads. `bundled` is the subset actually packed (skipped /
    // unreadable inputs excluded) — forwarded so finalize touches only
    // what was uploaded.
    let (zip_bytes, bundled) = tokio::task::spawn_blocking(move || build_zip(files, max_bytes))
        .await
        .context("collect: zip task join")??;

    let store = js
        .get_object_store(OBJECT_COLLECTIONS)
        .await
        .with_context(|| {
            format!(
                "collect: get_object_store {OBJECT_COLLECTIONS} — \
                 was bootstrap::ensure_jetstream_resources run on the backend?"
            )
        })?;

    // Key shape `<pc_id>/<job_id>/<ts>.zip` — the backend's
    // `parse_bundle_key` requires exactly these three segments and the
    // `.zip` suffix. Compact UTC timestamp (no ':' / '/') keeps the key
    // well-formed; millisecond precision (`%.3f`) so two runs of the same
    // job on the same PC in the same second don't collide and silently
    // overwrite each other's bundle (claude).
    let ts = now.format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let key = format!("{pc_id}/{}/{ts}.zip", cmd.id);
    let bytes_len = zip_bytes.len();
    let mut cursor = std::io::Cursor::new(zip_bytes);
    store
        .put(key.as_str(), &mut cursor)
        .await
        .with_context(|| format!("collect: object_store.put {key}"))?;
    info!(
        key,
        bytes = bytes_len,
        "collect: uploaded bundle to OBJECT_COLLECTIONS"
    );
    Ok((key, bundled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_list_reads_default_field() {
        let stdout = r#"{ "files": ["C:/a.txt", "C:/b.log"] }"#;
        let files = parse_file_list(stdout, "files").unwrap();
        assert_eq!(files, vec!["C:/a.txt".to_string(), "C:/b.log".to_string()]);
    }

    #[test]
    fn parse_file_list_honors_custom_field_and_skips_blanks() {
        let stdout = r#"{ "artifacts": ["x", "  ", "y"] }"#;
        let files = parse_file_list(stdout, "artifacts").unwrap();
        assert_eq!(files, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn parse_file_list_rejects_non_json_and_missing_field() {
        assert!(parse_file_list("not json", "files").is_err());
        assert!(parse_file_list(r#"{"other": []}"#, "files").is_err());
        assert!(parse_file_list(r#"{"files": "nope"}"#, "files").is_err());
    }

    #[test]
    fn build_zip_packs_files_and_dedups_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("report.txt");
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let b = sub.join("report.txt"); // same basename → must de-dup
        std::fs::write(&a, b"hello").unwrap();
        std::fs::write(&b, b"world").unwrap();

        let files = vec![
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ];
        let (zip, packed) = build_zip(files, 1_000_000).unwrap();
        // Both inputs were packed, so both come back in the bundled list.
        assert_eq!(packed.len(), 2);
        // Re-open the archive and confirm both entries survived under
        // distinct names.
        let reader = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        assert_eq!(reader.len(), 2);
        let names: Vec<String> = reader.file_names().map(|s| s.to_string()).collect();
        assert!(names.contains(&"report.txt".to_string()));
        assert!(names.iter().any(|n| n.ends_with("_report.txt")));
    }

    #[test]
    fn build_zip_enforces_max_size() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![0u8; 2048]).unwrap();
        let err = build_zip(vec![big.to_string_lossy().into_owned()], 1024).unwrap_err();
        assert!(err.to_string().contains("max_size"), "{err}");
    }

    #[test]
    fn build_zip_errors_when_no_readable_files() {
        let err = build_zip(vec!["C:/definitely/missing/xyz.txt".into()], 1_000_000).unwrap_err();
        assert!(err.to_string().contains("no readable files"), "{err}");
    }
}
