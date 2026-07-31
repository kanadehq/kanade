//! HTTP surface for collected file bundles (#219).
//!
//! A job carrying a `collect:` manifest hint prints a JSON list of file
//! paths on stdout; the agent zips those files and uploads the archive
//! to the [`OBJECT_COLLECTIONS`] Object Store bucket (key
//! `<pc_id>/<job_id>/<timestamp>.zip`). This module is the operator's
//! read side — the SPA Collect page lists / downloads / deletes bundles
//! straight from the bucket. There is NO upload endpoint here: bundles
//! are produced by agents via the normal exec result path, not POSTed.
//!
//! Endpoints (all under `/api/collect`):
//!
//! - `GET    /api/collect/bundles` — list every collected bundle.
//! - `GET    /api/collect/bundles/{*key}` — stream a bundle's bytes.
//! - `DELETE /api/collect/bundles/{*key}` — gc a bundle (operator).
//!
//! `{*key}` is a wildcard capturing the full `<pc_id>/<job_id>/<ts>.zip`
//! object key (it contains slashes, unlike the two-segment app-package
//! keys). Bundles auto-expire after the bucket's 30-day retention, so
//! delete is mostly for early cleanup.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use kanade_shared::kv::OBJECT_COLLECTIONS;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tokio_util::io::ReaderStream;
use tracing::warn;

use super::AppState;
use crate::audit;
use crate::audit::Caller;
use crate::projector::spec_cache::ExplodeSpecCache;

/// One collected bundle, as the SPA Collect page renders it. The
/// `name` / `description` are resolved from the producing job's
/// `collect:` hint (best-effort — `None` if the manifest was deleted
/// since the bundle was collected).
#[derive(Serialize)]
pub struct BundleRow {
    /// Full Object Store key (`<pc_id>/<job_id>/<ts>.zip`) — the
    /// download / delete path param.
    pub key: String,
    pub pc_id: String,
    pub job_id: String,
    /// The bundle's collection timestamp, taken from the key's filename
    /// (`<ts>` without the `.zip` suffix). Falls back to the object's
    /// `modified` time when the key isn't in the expected shape.
    pub collected_at: Option<String>,
    /// The bundle's label when the run produced multiple bundles (e.g. a
    /// per-day `"20260101"`); `None` for the single-bundle form.
    pub label: Option<String>,
    pub size: u64,
    pub digest: Option<String>,
    /// `collect.name` from the producing manifest, when still present.
    pub name: Option<String>,
    /// `collect.description` from the producing manifest.
    pub description: Option<String>,
}

/// Split a bundle key into its parts: `(pc_id, job_id, ts, label)`. The
/// filename is `<ts>.zip` (unlabeled, single-bundle form) or
/// `<label>__<ts>.zip` (one of a run's multiple labeled bundles, e.g. one
/// zip per day). Returns `None` when the key doesn't have the three
/// expected slash-separated segments (a stray / hand-uploaded object) so
/// the caller can skip it rather than render a garbage row.
fn parse_bundle_key(key: &str) -> Option<(String, String, String, Option<String>)> {
    let mut it = key.split('/');
    let pc_id = it.next()?;
    let job_id = it.next()?;
    let filename = it.next()?;
    // Exactly three segments and a `.zip` filename — enforce the
    // documented contract so a stray / hand-uploaded object becomes a
    // skipped `None` instead of a garbage row (coderabbit).
    if pc_id.is_empty() || job_id.is_empty() || it.next().is_some() {
        return None;
    }
    let stem = filename.strip_suffix(".zip")?;
    if stem.is_empty() {
        return None;
    }
    // `<label>__<ts>` when a label prefix is present (the agent sanitizes
    // labels so they never contain `__`, and the timestamp has none), else
    // the whole stem is the timestamp. Split on the LAST `__` so a stray
    // double-underscore can't shadow the real separator.
    let (label, ts) = match stem.rsplit_once("__") {
        Some((l, t)) if !l.is_empty() && !t.is_empty() => (Some(l.to_string()), t.to_string()),
        _ => (None, stem.to_string()),
    };
    Some((pc_id.to_string(), job_id.to_string(), ts, label))
}

/// Best-effort map of `job_id → (collect.name, collect.description)` for
/// the jobs that actually produced bundles. Reads the backend's
/// in-memory `BUCKET_JOBS` cache (kept fresh by a `watch_all()` watcher —
/// see [`ExplodeSpecCache`]) instead of a per-job `kv.get`. The old KV
/// walk was an N+1 broker round-trip whose latency grew with the *total*
/// job count, dominating the SPA Collect page's first paint on a large
/// fleet; the cache read is an in-memory lock acquisition per distinct
/// job, so the listing is essentially free. A cache miss (job
/// unregistered, or created within the watcher's ~1 s sync window) just
/// leaves that bundle unlabelled — same best-effort contract the KV walk
/// had.
async fn collect_job_meta(
    cache: &ExplodeSpecCache,
    job_ids: &HashSet<&str>,
) -> HashMap<String, (String, Option<String>)> {
    let mut map = HashMap::new();
    for &job_id in job_ids {
        let Some(manifest) = cache.manifest(job_id).await else {
            continue;
        };
        if let Some(hint) = &manifest.collect {
            // Only allocate the owned key for jobs that actually resolve.
            map.insert(
                job_id.to_string(),
                (hint.name.clone(), hint.description.clone()),
            );
        }
    }
    map
}

// ─── GET /api/collect/bundles ────────────────────────────────────────

pub async fn list_bundles(
    State(state): State<AppState>,
) -> Result<Json<Vec<BundleRow>>, (StatusCode, String)> {
    // #1216: read the SQLite metadata index (projector::object_meta)
    // instead of ObjectStore::list() — the latter full-scanned the
    // stream per cold request (29.6 s for 20 bundles on the measured
    // bucket). Agent uploads never touch the backend API; the index
    // sees them via the bucket's metadata watch.
    let metas = crate::projector::object_meta::list_bucket(&state.pool, OBJECT_COLLECTIONS)
        .await
        .map_err(|e| {
            warn!(error = %e, "object_store_meta list collections");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    // First pass: parse every object key into a row skeleton. The
    // distinct producing job_ids are gathered from `rows` afterwards so
    // we can resolve their `collect:` labels from the in-memory jobs
    // cache in one shot (below) rather than a KV round-trip per job.
    let mut rows = Vec::new();
    for m in metas {
        let Some((pc_id, job_id, collected_at, label)) = parse_bundle_key(&m.key) else {
            warn!(key = %m.key, "collect.list: object key not <pc_id>/<job_id>/[<label>__]<ts>.zip — skipping");
            continue;
        };
        rows.push(BundleRow {
            key: m.key,
            pc_id,
            job_id,
            // Prefer the key's own timestamp; fall back to the object's
            // modified time only when the filename wasn't a timestamp.
            collected_at: Some(collected_at).filter(|s| !s.is_empty()).or(m.modified),
            label,
            size: m.size as u64,
            digest: m.digest,
            // Filled in from the jobs cache in the second pass, once we
            // know every job_id that appears in the listing.
            name: None,
            description: None,
        });
    }

    // Second pass: label each bundle from its producing job's `collect:`
    // hint, read straight from the in-memory cache (no KV round-trip).
    // Borrow the distinct job_ids straight out of `rows` — no per-bundle
    // String clone; only the jobs that actually resolve get an owned key
    // (gemini).
    let job_ids: HashSet<&str> = rows.iter().map(|row| row.job_id.as_str()).collect();
    let job_meta = collect_job_meta(&state.explode_spec_cache, &job_ids).await;
    for row in &mut rows {
        if let Some((name, description)) = job_meta.get(&row.job_id) {
            row.name = Some(name.clone());
            row.description = description.clone();
        }
    }

    // Newest first, then by key for a deterministic tie-break.
    rows.sort_by(|a, b| {
        b.collected_at
            .cmp(&a.collected_at)
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(Json(rows))
}

// ─── GET /api/collect/bundles/{*key} ─────────────────────────────────

pub async fn download_bundle(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    if key.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "bundle key must be non-empty".into(),
        ));
    }
    let store = state
        .jetstream
        .get_object_store(OBJECT_COLLECTIONS)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let obj = match store.get(key.as_str()).await {
        Ok(o) => o,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("no objects") {
                return Err((StatusCode::NOT_FOUND, format!("bundle '{key}' not found")));
            }
            warn!(error = %e, %key, "object_store.get collections");
            return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
        }
    };

    let total_size = obj.info().size as u64;
    let etag = obj.info().digest.clone().map(|d| format!("\"{d}\""));

    // Honour If-Match so a client resuming across a re-collection
    // refuses mid-flight drift (same posture as app-packages). `*`
    // matches any existing representation per RFC 7232 §3.1, so it must
    // pass once we've confirmed the object exists (claude).
    if let Some(ref expected) = etag
        && let Some(if_match) = headers.get(header::IF_MATCH)
        && let Ok(s) = if_match.to_str()
        && s != "*"
        && s != expected
    {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            format!("If-Match {s:?} doesn't match current ETag {expected:?}"),
        ));
    }

    // Suggest a flat, key-safe filename: `<pc_id>-<job_id>-<ts>.zip`.
    // Swap '/' for '-', and defensively map '\' / '"' to '_' so a
    // malformed / hand-uploaded key can't break the Content-Disposition
    // quoted-string (gemini security). Single pass to keep clippy happy.
    let suggested_filename: String = key
        .chars()
        .map(|c| match c {
            '/' => '-',
            '\\' | '"' => '_',
            other => other,
        })
        .collect();

    let mut resp = (
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_LENGTH, total_size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{suggested_filename}\""),
            ),
        ],
        Body::from_stream(ReaderStream::new(obj)),
    )
        .into_response();
    if let Some(etag) = etag
        && let Ok(v) = etag.parse()
    {
        resp.headers_mut().insert(header::ETAG, v);
    }
    Ok(resp)
}

// ─── DELETE /api/collect/bundles/{*key} ──────────────────────────────

pub async fn delete_bundle(
    State(state): State<AppState>,
    Path(key): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    if key.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "bundle key must be non-empty".into(),
        ));
    }
    let store = state
        .jetstream
        .get_object_store(OBJECT_COLLECTIONS)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    store.delete(key.as_str()).await.map_err(|e| {
        warn!(error = %e, %key, "object_store.delete collections");
        let msg = e.to_string();
        if msg.contains("not found") || msg.contains("no objects") {
            (
                StatusCode::NOT_FOUND,
                format!("bundle '{key}' not in Object Store"),
            )
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    })?;

    // #1216: write-through so the SPA's immediate post-delete refetch
    // no longer lists the bundle (watcher would heal, but slower).
    if let Err(e) =
        crate::projector::object_meta::delete_key(&state.pool, OBJECT_COLLECTIONS, &key).await
    {
        warn!(error = %e, %key, "object_meta write-through failed (watcher will heal)");
    }

    audit::record(
        &state.nats,
        "operator",
        "collect_bundle_delete",
        Some(&key),
        Some(&caller),
        serde_json::json!({ "key": key }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::manifest::Manifest;

    /// Minimal valid manifest, optionally carrying a `collect:` hint.
    /// Pass `serde_json::Value::Null` for `collect` to model a job that
    /// produced a bundle-shaped key but has no collect hint.
    fn manifest(id: &str, collect: serde_json::Value) -> Manifest {
        let mut base = serde_json::json!({
            "id": id,
            "version": "0.0.1",
            "execute": {
                "shell": "powershell",
                "script": "echo '{}'",
                "timeout": "30s",
            },
        });
        if !collect.is_null() {
            base.as_object_mut()
                .expect("object")
                .insert("collect".into(), collect);
        }
        serde_json::from_value(base).expect("fixture manifest parses")
    }

    #[tokio::test]
    async fn collect_job_meta_resolves_hits_skips_miss_and_no_hint() {
        let cache = ExplodeSpecCache::new();
        // Full collect hint (name + description).
        cache
            .insert_manifest(manifest(
                "collect-logs",
                serde_json::json!({ "name": "Logs", "description": "app logs" }),
            ))
            .await;
        // Collect hint with no description.
        cache
            .insert_manifest(manifest(
                "collect-min",
                serde_json::json!({ "name": "Minimal" }),
            ))
            .await;
        // Cached but WITHOUT a collect hint — must not be labelled.
        cache
            .insert_manifest(manifest("plain-job", serde_json::Value::Null))
            .await;

        // "ghost" is in the listing but not the cache (unregistered job,
        // or created within the watcher's sync window) — a silent miss.
        let job_ids: HashSet<&str> = ["collect-logs", "collect-min", "plain-job", "ghost"]
            .into_iter()
            .collect();
        let meta = collect_job_meta(&cache, &job_ids).await;

        assert_eq!(
            meta.get("collect-logs"),
            Some(&("Logs".to_string(), Some("app logs".to_string())))
        );
        assert_eq!(
            meta.get("collect-min"),
            Some(&("Minimal".to_string(), None))
        );
        assert!(
            !meta.contains_key("plain-job"),
            "job without a collect hint stays unlabelled"
        );
        assert!(
            !meta.contains_key("ghost"),
            "cache miss stays unlabelled (best-effort)"
        );
        assert_eq!(meta.len(), 2);
    }

    #[test]
    fn parse_bundle_key_splits_three_segments() {
        // Unlabeled (single-bundle) form: label is None.
        assert_eq!(
            parse_bundle_key("PC001/collect-diagnostics/20260615T220000Z.zip"),
            Some((
                "PC001".to_string(),
                "collect-diagnostics".to_string(),
                "20260615T220000Z".to_string(),
                None,
            )),
        );
    }

    #[test]
    fn parse_bundle_key_extracts_label_from_multi_bundle_filename() {
        // Labeled (multi-bundle) form: `<label>__<ts>.zip`.
        assert_eq!(
            parse_bundle_key("PC001/screenshot-collect/20260101__20260625T220000.123Z.zip"),
            Some((
                "PC001".to_string(),
                "screenshot-collect".to_string(),
                "20260625T220000.123Z".to_string(),
                Some("20260101".to_string()),
            )),
        );
        // A leading/trailing empty half around `__` is not a real label —
        // treat the whole stem as the timestamp.
        assert_eq!(
            parse_bundle_key("pc/job/__ts.zip"),
            Some((
                "pc".to_string(),
                "job".to_string(),
                "__ts".to_string(),
                None
            )),
        );
    }

    #[test]
    fn parse_bundle_key_rejects_short_or_empty() {
        assert_eq!(parse_bundle_key("PC001/job"), None);
        assert_eq!(parse_bundle_key("PC001"), None);
        assert_eq!(parse_bundle_key("//x.zip"), None);
        assert_eq!(parse_bundle_key(""), None);
    }

    #[test]
    fn parse_bundle_key_enforces_zip_and_three_segments() {
        // A non-.zip filename violates the contract → skipped.
        assert_eq!(parse_bundle_key("pc/job/raw-name"), None);
        // Empty timestamp (just ".zip") → skipped.
        assert_eq!(parse_bundle_key("pc/job/.zip"), None);
        // A fourth segment (extra slash content) → skipped.
        assert_eq!(parse_bundle_key("pc/job/extra/ts.zip"), None);
    }
}
