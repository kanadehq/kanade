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

/// One uploaded bundle: its label (the multi-bundle key segment, `None`
/// for the single-bundle back-compat form), the Object Store key, and the
/// input paths actually packed (for the finalize hook to act on exactly
/// what was uploaded).
pub struct BundleResult {
    pub label: Option<String>,
    pub key: String,
    pub files: Vec<String>,
}

/// If `cmd` carries a `collect:` hint, build + upload its bundle(s) and
/// return one [`BundleResult`] per uploaded zip. A job emits either a
/// single `files` list (one unlabeled bundle) or a `bundles` array (one
/// zip per labeled bundle — e.g. one per day). Best-effort: any failure
/// is logged and yields an empty Vec so the run's `ExecResult` still
/// publishes; an individual over-size / unreadable bundle is skipped
/// while the others upload. Caller should only invoke this on a clean
/// (`exit_code == 0`) run.
///
/// The returned file lists let the finalize hook act on exactly the
/// uploaded files (e.g. delete only what was bundled). An empty Vec (no
/// hint, total failure) means nothing was uploaded.
pub async fn maybe_collect(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    cmd: &Command,
    pc_id: &str,
    parent_result_id: &str,
    stdout: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<BundleResult> {
    let Some(hint) = cmd.collect.as_ref() else {
        return Vec::new();
    };
    match collect_and_upload(js, client, cmd, hint, pc_id, parent_result_id, stdout, now).await {
        Ok(bundles) => {
            info!(job = %cmd.id, %pc_id, bundles = bundles.len(), "collect: bundle(s) uploaded");
            bundles
        }
        Err(e) => {
            warn!(error = %e, job = %cmd.id, %pc_id, "collect: bundle failed; publishing result without a bundle");
            Vec::new()
        }
    }
}

/// A bundle requested by the script: an optional label and its file list.
struct RequestedBundle {
    label: Option<String>,
    files: Vec<String>,
}

/// Sanitize a bundle label into one safe key segment: keep
/// `[A-Za-z0-9.-]`, map everything else (including `_`, so the `__`
/// label/timestamp separator in the key stays unambiguous) to `-`. An
/// empty result falls back to `bundle`.
fn sanitize_label(label: &str) -> String {
    let s: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "bundle".to_string()
    } else {
        s
    }
}

/// Ensure a bundle's (already-sanitized) label is unique within a run.
/// Records it in `used`; on a collision appends `-N` until distinct. The
/// set key for an unlabeled (`None`) bundle is `""`, so the FIRST
/// unlabeled bundle stays `None` (preserving the `<pc>/<job>/<ts>.zip`
/// back-compat shape) and a second unlabeled bundle becomes `bundle-2`.
fn dedup_label(
    used: &mut std::collections::HashSet<String>,
    label: Option<String>,
) -> Option<String> {
    let base = label.clone().unwrap_or_default();
    if used.insert(base.clone()) {
        return label;
    }
    let stem = if base.is_empty() {
        "bundle".to_string()
    } else {
        base
    };
    let mut n = 2;
    loop {
        let cand = format!("{stem}-{n}");
        if used.insert(cand.clone()) {
            return Some(cand);
        }
        n += 1;
    }
}

/// Pull a non-empty-string array out of a JSON value (the `files` field).
fn json_string_array(value: Option<&serde_json::Value>, field: &str) -> Result<Vec<String>> {
    let arr = value
        .with_context(|| format!("collect: stdout JSON has no '{field}'"))?
        .as_array()
        .with_context(|| format!("collect: '{field}' is not an array"))?;
    let mut files = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .with_context(|| format!("collect: '{field}' entry is not a string"))?;
        if !s.trim().is_empty() {
            files.push(s.to_string());
        }
    }
    Ok(files)
}

/// Parse the collect payload into one or more requested bundles. Multi-
/// bundle form: `{ "bundles": [ {"label": "20260101", "files": [...]}, …]}`
/// → one zip per element. Single-bundle back-compat: `{ "<files_field>":
/// [...] }` → one unlabeled bundle.
fn parse_bundles(payload: &str, files_field: &str) -> Result<Vec<RequestedBundle>> {
    let value: serde_json::Value =
        serde_json::from_str(payload.trim()).context("collect: stdout is not valid JSON")?;
    if let Some(arr) = value.get("bundles").and_then(|b| b.as_array()) {
        let mut out = Vec::with_capacity(arr.len());
        for (i, b) in arr.iter().enumerate() {
            let label = b
                .get("label")
                .and_then(|l| l.as_str())
                .map(|s| s.to_string());
            let files = json_string_array(b.get("files"), &format!("bundles[{i}].files"))?;
            out.push(RequestedBundle { label, files });
        }
        Ok(out)
    } else {
        // Single-bundle back-compat. Reuse the already-parsed `value`
        // rather than re-parsing the payload (gemini).
        let files = json_string_array(value.get(files_field), files_field)?;
        Ok(vec![RequestedBundle { label: None, files }])
    }
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

#[allow(clippy::too_many_arguments)]
async fn collect_and_upload(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    cmd: &Command,
    hint: &CollectHint,
    pc_id: &str,
    parent_result_id: &str,
    stdout: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<BundleResult>> {
    // #821: read collect's own fenced block so a job can also carry a
    // user message and/or other hints' blocks on the same stdout. No
    // fence ⇒ the whole stdout (back-compat for a collect-only job).
    let payload = kanade_shared::manifest::fenced_payload(
        stdout,
        kanade_shared::manifest::COLLECT_BLOCK_BEGIN,
        kanade_shared::manifest::COLLECT_BLOCK_END,
    );
    let requested = parse_bundles(payload, &hint.files_field)?;
    if requested.iter().all(|b| b.files.is_empty()) {
        bail!("collect: file list is empty");
    }
    let max_bytes = hint.max_size_bytes();

    let store = js
        .get_object_store(OBJECT_COLLECTIONS)
        .await
        .with_context(|| {
            format!(
                "collect: get_object_store {OBJECT_COLLECTIONS} — \
                 was bootstrap::ensure_jetstream_resources run on the backend?"
            )
        })?;

    // All of one run's bundles share the run's UTC timestamp; the label
    // (when present) makes each key distinct. Compact timestamp (no ':' /
    // '/'), millisecond precision so two runs of the same job on the same
    // PC in the same second don't collide.
    let ts = now.format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let mut out: Vec<BundleResult> = Vec::new();
    // De-dup labels within the run so two bundles whose labels collapse to
    // the same sanitized segment (e.g. "2026_01" and "2026-01" both →
    // "2026-01") don't build the same key and silently overwrite each
    // other's zip (claude).
    let mut used_labels: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (bundle_idx, b) in requested.into_iter().enumerate() {
        if b.files.is_empty() {
            continue;
        }
        let label = b.label.as_deref().map(sanitize_label);
        let files = b.files;

        // File reads + zip compression are blocking; keep them off the
        // async worker threads. An over-size / unreadable bundle is
        // skipped (logged) so it can't wedge the whole run — the other
        // bundles still upload, and a too-big day is retried next run.
        let built = tokio::task::spawn_blocking(move || build_zip(files, max_bytes))
            .await
            .context("collect: zip task join")?;
        let (zip_bytes, packed) = match built {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, label = ?label, "collect: bundle skipped (zip failed)");
                continue;
            }
        };

        // De-dup against labels already used THIS run (only now that the
        // zip built — a skipped bundle mustn't reserve a label). A first
        // unlabeled bundle stays unlabeled (back-compat); a colliding one
        // gets a `-N` suffix so its key is distinct.
        let label = dedup_label(&mut used_labels, label);

        // Key shape: `<pc_id>/<job_id>/<label>__<ts>.zip` (labeled) or
        // `<pc_id>/<job_id>/<ts>.zip` (unlabeled, back-compat) — exactly
        // three slash segments, which the backend's `parse_bundle_key`
        // requires; the filename carries the optional `<label>__` prefix.
        let key = match &label {
            Some(l) => format!("{pc_id}/{}/{l}__{ts}.zip", cmd.id),
            None => format!("{pc_id}/{}/{ts}.zip", cmd.id),
        };
        let bytes_len = zip_bytes.len();
        let mut cursor = std::io::Cursor::new(zip_bytes);
        if let Err(e) = store.put(key.as_str(), &mut cursor).await {
            warn!(error = %e, key, "collect: upload failed; skipping bundle");
            continue;
        }
        info!(
            key,
            bytes = bytes_len,
            "collect: uploaded bundle to OBJECT_COLLECTIONS"
        );
        let bundle = BundleResult {
            label,
            key,
            files: packed,
        };

        // #965: per-bundle finalize. Run the cleanup hook for THIS bundle
        // the instant its zip is safely uploaded — so if collect is cut
        // off later (the endpoint sleeps / disconnects), the days already
        // shipped are still cleaned up (partial progress sticks), instead
        // of the whole run's cleanup being lost because the one aggregate
        // hook after the full set never ran. Opt-in via
        // `finalize.on_each_bundle`; when off, the caller runs the single
        // aggregate hook after collect returns (the established contract).
        if let Some(fin) = cmd.finalize.as_ref()
            && fin.on_each_bundle
        {
            let one = crate::finalize::collect_result_json(std::slice::from_ref(&bundle));
            // #966: the bundle index discriminates the finalize row's
            // synthetic request_id (→ outbox filename) so per-bundle rows
            // don't clobber each other on disk before the drain publishes.
            let disc = bundle_idx.to_string();
            crate::finalize::run_finalize(
                client,
                cmd,
                fin,
                pc_id,
                parent_result_id,
                Some(&disc),
                Some(one.as_str()),
            )
            .await;
        }

        out.push(bundle);
    }

    if out.is_empty() {
        bail!("collect: no bundles uploaded");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_array_reads_and_skips_blanks() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{ "artifacts": ["x", "  ", "y"] }"#).unwrap();
        let files = json_string_array(v.get("artifacts"), "artifacts").unwrap();
        assert_eq!(files, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn parse_bundles_honors_custom_files_field() {
        let b = parse_bundles(r#"{ "artifacts": ["x", "y"] }"#, "artifacts").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].files, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn parse_bundles_rejects_non_json_and_missing_or_non_array_field() {
        assert!(parse_bundles("not json", "files").is_err());
        assert!(parse_bundles(r#"{"other": []}"#, "files").is_err());
        assert!(parse_bundles(r#"{"files": "nope"}"#, "files").is_err());
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

    #[test]
    fn parse_bundles_single_files_form_is_one_unlabeled_bundle() {
        let b = parse_bundles(r#"{ "files": ["C:/a.png", "C:/b.png"] }"#, "files").unwrap();
        assert_eq!(b.len(), 1);
        assert!(b[0].label.is_none());
        assert_eq!(b[0].files, vec!["C:/a.png", "C:/b.png"]);
    }

    #[test]
    fn parse_bundles_multi_form_keeps_labels_and_files() {
        let stdout = r#"{ "bundles": [
            { "label": "20260101", "files": ["C:/1/a.png"] },
            { "label": "20260102", "files": ["C:/2/a.png", "C:/2/b.png"] }
        ] }"#;
        let b = parse_bundles(stdout, "files").unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].label.as_deref(), Some("20260101"));
        assert_eq!(b[1].files.len(), 2);
    }

    #[test]
    fn sanitize_label_strips_unsafe_chars_and_underscores() {
        // `_` becomes `-` so the `__` key separator stays unambiguous.
        assert_eq!(sanitize_label("2026_01/02:03"), "2026-01-02-03");
        assert_eq!(sanitize_label("daily"), "daily");
        assert_eq!(sanitize_label("###"), "---");
    }

    #[test]
    fn dedup_label_disambiguates_sanitization_collisions() {
        let mut used = std::collections::HashSet::new();
        // Two labels that sanitize to the same segment must not collide.
        assert_eq!(
            dedup_label(&mut used, Some("2026-01".into())),
            Some("2026-01".into())
        );
        assert_eq!(
            dedup_label(&mut used, Some("2026-01".into())),
            Some("2026-01-2".into())
        );
        // First unlabeled stays None (back-compat key); a second gets a
        // synthetic suffix so it can't clobber the first.
        let mut u2 = std::collections::HashSet::new();
        assert_eq!(dedup_label(&mut u2, None), None);
        assert_eq!(dedup_label(&mut u2, None), Some("bundle-2".into()));
    }
}
