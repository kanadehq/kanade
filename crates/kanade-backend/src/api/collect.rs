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
use futures::StreamExt;
use kanade_shared::kv::{BUCKET_JOBS, OBJECT_COLLECTIONS};
use kanade_shared::manifest::Manifest;
use serde::Serialize;
use std::collections::HashMap;
use tokio_util::io::ReaderStream;
use tracing::warn;

use super::AppState;
use crate::audit;
use crate::audit::Caller;

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
    pub size: u64,
    pub digest: Option<String>,
    /// `collect.name` from the producing manifest, when still present.
    pub name: Option<String>,
    /// `collect.description` from the producing manifest.
    pub description: Option<String>,
}

/// Split a bundle key `<pc_id>/<job_id>/<ts>.zip` into its parts.
/// Returns `None` when the key doesn't have the three expected
/// slash-separated segments (a stray / hand-uploaded object) so the
/// caller can skip it rather than render a garbage row.
fn parse_bundle_key(key: &str) -> Option<(String, String, String)> {
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
    let collected_at = filename.strip_suffix(".zip")?;
    if collected_at.is_empty() {
        return None;
    }
    Some((
        pc_id.to_string(),
        job_id.to_string(),
        collected_at.to_string(),
    ))
}

/// Best-effort map of `job_id → (collect.name, collect.description)`
/// by walking `BUCKET_JOBS` once. Used to label bundles with their
/// producing job's human-facing hint. A KV miss / parse error just
/// leaves a bundle unlabelled rather than failing the whole listing.
async fn collect_job_meta(state: &AppState) -> HashMap<String, (String, Option<String>)> {
    let mut map = HashMap::new();
    let Ok(kv) = state.jetstream.get_key_value(BUCKET_JOBS).await else {
        return map;
    };
    let Ok(mut keys) = kv.keys().await else {
        return map;
    };
    // Collect the keys first, then fetch concurrently — a sequential
    // `kv.get` per job is an N+1 round-trip that scales latency with the
    // job count (gemini). PR3 can replace this with a tap into the
    // in-memory BUCKET_JOBS watch the backend already keeps, making the
    // listing essentially free.
    let mut key_list = Vec::new();
    while let Some(key) = keys.next().await {
        if let Ok(key) = key {
            key_list.push(key);
        }
    }
    let fetched = futures::future::join_all(key_list.into_iter().map(|key| {
        let kv = kv.clone();
        async move {
            match kv.get(&key).await {
                Ok(Some(entry)) => serde_json::from_slice::<Manifest>(&entry).ok(),
                _ => None,
            }
        }
    }))
    .await;
    for job in fetched.into_iter().flatten() {
        if let Some(hint) = job.collect {
            map.insert(job.id, (hint.name, hint.description));
        }
    }
    map
}

// ─── GET /api/collect/bundles ────────────────────────────────────────

pub async fn list_bundles(
    State(state): State<AppState>,
) -> Result<Json<Vec<BundleRow>>, (StatusCode, String)> {
    let store = state
        .jetstream
        .get_object_store(OBJECT_COLLECTIONS)
        .await
        .map_err(|e| {
            warn!(error = %e, "get_object_store collections");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Object Store '{OBJECT_COLLECTIONS}' missing — run `kanade jetstream setup`"
                ),
            )
        })?;
    let mut list = store.list().await.map_err(|e| {
        warn!(error = %e, "object_store.list collections");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let job_meta = collect_job_meta(&state).await;

    let mut rows = Vec::new();
    while let Some(item) = list.next().await {
        // Propagate stream errors rather than truncating — a partial
        // list served as 200 would lie about what's in the bucket.
        let meta = item.map_err(|e| {
            warn!(error = %e, "collect.list: object metadata stream error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list bundles: {e}"),
            )
        })?;
        let Some((pc_id, job_id, collected_at)) = parse_bundle_key(&meta.name) else {
            warn!(key = %meta.name, "collect.list: object key not <pc_id>/<job_id>/<ts>.zip — skipping");
            continue;
        };
        let modified = meta
            .modified
            .and_then(|t| chrono::DateTime::from_timestamp(t.unix_timestamp(), t.nanosecond()))
            .map(|d| d.to_rfc3339());
        let (name, description) = match job_meta.get(&job_id) {
            Some((n, d)) => (Some(n.clone()), d.clone()),
            None => (None, None),
        };
        rows.push(BundleRow {
            key: meta.name,
            pc_id,
            job_id,
            // Prefer the key's own timestamp; fall back to the object's
            // modified time only when the filename wasn't a timestamp.
            collected_at: Some(collected_at).filter(|s| !s.is_empty()).or(modified),
            size: meta.size as u64,
            digest: meta.digest,
            name,
            description,
        });
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

    #[test]
    fn parse_bundle_key_splits_three_segments() {
        assert_eq!(
            parse_bundle_key("PC001/collect-diagnostics/20260615T220000Z.zip"),
            Some((
                "PC001".to_string(),
                "collect-diagnostics".to_string(),
                "20260615T220000Z".to_string(),
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
