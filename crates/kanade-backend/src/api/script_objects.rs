//! HTTP surface for the manifest-script Object Store
//! (`OBJECT_SCRIPTS` bucket).
//!
//! Sibling of `api::app_packages` — same shape (CRUD over an
//! Object Store, keyed by `<name>/<version>`, Range + ETag +
//! If-Match download). Distinct module because the two buckets
//! have different lifecycles (see `kanade-shared::kv::OBJECT_SCRIPTS`
//! for the split rationale).
//!
//! Naming: this module is `script_objects` (plural) to match the
//! SPEC §2.4.1 `Execute::script_object` field name and to avoid
//! a collision with `api::scripts`, which already owns the
//! manifest-script revoke/unrevoke flow (`script_current` +
//! `script_status` KV buckets). The route prefix is
//! `/api/script-objects`.
//!
//! Endpoints:
//!
//! - `GET    /api/script-objects` — list every script + version.
//! - `POST   /api/script-objects/{name}/{version}` — multipart
//!   upload (`file = <script bytes>`), replaces any existing
//!   object at the same key.
//! - `GET    /api/script-objects/{name}/{version}` — stream the
//!   stored bytes. Used by kanade-agent during Execute resolution
//!   when `Command::script_object` is set (follow-up PR; this PR
//!   only ships the storage surface).
//! - `DELETE /api/script-objects/{name}/{version}` — gc.
//!
//! Code duplication note: the body of every handler mirrors
//! `api::app_packages` almost verbatim. Kept duplicated on
//! purpose — the rule-of-three says abstract on the third
//! consumer (diagnostics bucket, deferred). At two buckets, the
//! abstraction shape isn't settled enough to extract without
//! premature bending.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use kanade_shared::kv::OBJECT_SCRIPTS;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

use super::AppState;
use crate::audit;
use crate::audit::Caller;

/// Compose the Object Store object name for a (name, version)
/// pair. Lives in one place so the upload / download / delete
/// paths can't accidentally drift apart.
fn object_key(name: &str, version: &str) -> String {
    format!("{name}/{version}")
}

/// Validate the path-param halves are usable as Object Store key
/// segments AND as raw substrings of an HTTP
/// `Content-Disposition: attachment; filename="…"` header.
///
/// Same restrictions as `api::app_packages::validate_segment`:
/// non-empty, no `/`, ASCII-printable, no `"` / `\`. Common
/// manifest ids / versions (`inventory-os`, `2025.03`,
/// `7c6f9d4a`) all pass.
fn validate_segment(label: &str, value: &str) -> Result<(), (StatusCode, String)> {
    if value.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{label} must be non-empty"),
        ));
    }
    if value.contains('/') {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{label} must not contain '/'"),
        ));
    }
    for c in value.chars() {
        if !c.is_ascii() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{label} must be ASCII-printable (rejected non-ASCII character {c:?})"),
            ));
        }
        if c.is_ascii_control() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{label} must not contain control characters"),
            ));
        }
        if c == '"' || c == '\\' {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{label} must not contain '\"' or '\\\\'"),
            ));
        }
    }
    Ok(())
}

// ─── POST /api/script-objects/{name}/{version} ───────────────────────

#[derive(Serialize)]
pub struct PublishResponse {
    pub name: String,
    pub version: String,
    pub size: u64,
    pub digest: Option<String>,
}

pub async fn publish(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
    caller: Caller,
    mut multipart: Multipart,
) -> Result<Json<PublishResponse>, (StatusCode, String)> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;

    // Same `Bytes`-not-Vec<u8> rationale as app_packages — see
    // that module for the double-allocation discussion. Scripts
    // are bounded by `SCRIPT_OBJECT_BODY_LIMIT` (4 MB), so the
    // win is smaller than for installer payloads, but keeping
    // the code shape identical helps the eventual rule-of-three
    // helper extract.
    let mut bytes: Option<Bytes> = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("read multipart field: {e}"),
        )
    })? {
        match field.name().unwrap_or("") {
            "file" => {
                bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| (StatusCode::BAD_REQUEST, format!("read file field: {e}")))?,
                );
            }
            other => {
                warn!(
                    field = other,
                    "script_objects.publish: ignoring unknown multipart field"
                );
            }
        }
    }
    let bytes = bytes.ok_or((StatusCode::BAD_REQUEST, "missing 'file' field".into()))?;
    if bytes.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "'file' field is empty".into()));
    }

    let size = bytes.len() as u64;
    let key = object_key(&name, &version);
    info!(name, version, size, key, "script_objects: uploading");

    let store = state
        .jetstream
        .get_object_store(OBJECT_SCRIPTS)
        .await
        .map_err(|e| {
            warn!(error = %e, "get_object_store scripts");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Object Store '{OBJECT_SCRIPTS}' missing — run `kanade jetstream setup`"),
            )
        })?;
    let mut cursor = std::io::Cursor::new(bytes);
    let meta = store.put(key.as_str(), &mut cursor).await.map_err(|e| {
        warn!(error = %e, %key, "object_store.put");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    info!(name, version, digest = ?meta.digest, "script_objects: uploaded");

    // #1216: write-through so the SPA's immediate post-upload refetch
    // sees the object without racing the metadata watcher. Best-effort
    // — the watcher heals the index on the meta message.
    if let Err(e) = crate::projector::object_meta::apply(&state.pool, OBJECT_SCRIPTS, &meta).await {
        warn!(error = %e, %key, "object_meta write-through failed (watcher will heal)");
    }

    audit::record(
        &state.nats,
        "operator",
        "script_object_publish",
        Some(&key),
        Some(&caller),
        serde_json::json!({
            "name": name,
            "version": version,
            "size": size,
            "digest": meta.digest,
        }),
    )
    .await;

    Ok(Json(PublishResponse {
        name,
        version,
        size,
        digest: meta.digest,
    }))
}

// ─── GET /api/script-objects ─────────────────────────────────────────

#[derive(Serialize)]
pub struct ScriptObjectRow {
    pub name: String,
    pub version: String,
    pub size: u64,
    pub digest: Option<String>,
    /// RFC3339 timestamp from the Object Store metadata.
    pub modified: Option<String>,
}

pub async fn list_objects(
    State(state): State<AppState>,
) -> Result<Json<Vec<ScriptObjectRow>>, (StatusCode, String)> {
    // #1216: read the SQLite metadata index (projector::object_meta)
    // instead of ObjectStore::list() — same full-stream scan the
    // sibling endpoints paid, just not (yet) user-visible on this
    // small bucket.
    let metas = crate::projector::object_meta::list_bucket(&state.pool, OBJECT_SCRIPTS)
        .await
        .map_err(|e| {
            warn!(error = %e, "object_store_meta list scripts");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    let mut rows = Vec::new();
    for m in metas {
        let (name, version) = match m.key.rsplit_once('/') {
            Some((n, v)) => (n.to_string(), v.to_string()),
            None => {
                warn!(key = %m.key, "script_objects.list: object key has no '/' — skipping");
                continue;
            }
        };
        rows.push(ScriptObjectRow {
            name,
            version,
            size: m.size as u64,
            digest: m.digest,
            modified: m.modified,
        });
    }
    rows.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.version.cmp(&b.version))
    });
    Ok(Json(rows))
}

// ─── GET /api/script-objects/{name}/{version} ────────────────────────

/// Parsed `Range:` header. `bytes=N-` (suffix-less open) and
/// `bytes=N-M` (closed interval) supported; multipart ranges and
/// suffix-only `bytes=-N` (last N bytes) are not — same scope as
/// `api::app_packages::ByteRange`.
#[derive(Debug, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum RangeResult {
    None,
    Valid(ByteRange),
    Invalid,
}

fn parse_range(header: Option<&str>, total_size: u64) -> RangeResult {
    let Some(h) = header else {
        return RangeResult::None;
    };
    let Some(bytes) = h.strip_prefix("bytes=") else {
        return RangeResult::Invalid;
    };
    let Some((start_str, end_str)) = bytes.split_once('-') else {
        return RangeResult::Invalid;
    };
    if start_str.is_empty() {
        return RangeResult::Invalid;
    }
    let Ok(start) = start_str.parse::<u64>() else {
        return RangeResult::Invalid;
    };
    let end = if end_str.is_empty() {
        None
    } else {
        let Ok(e) = end_str.parse::<u64>() else {
            return RangeResult::Invalid;
        };
        Some(e)
    };
    if start >= total_size {
        return RangeResult::Invalid;
    }
    if let Some(e) = end
        && (e >= total_size || e < start)
    {
        return RangeResult::Invalid;
    }
    RangeResult::Valid(ByteRange { start, end })
}

pub async fn download(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;

    let key = object_key(&name, &version);
    let store = state
        .jetstream
        .get_object_store(OBJECT_SCRIPTS)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let mut obj = match store.get(key.as_str()).await {
        Ok(o) => o,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("no objects") {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("script object '{name}/{version}' not found"),
                ));
            }
            warn!(error = %e, %key, "object_store.get");
            return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
        }
    };

    let total_size = obj.info().size as u64;
    let digest = obj.info().digest.clone();

    let etag = digest.as_deref().map(|d| format!("\"{d}\""));
    if let Some(ref expected) = etag
        && let Some(if_match) = headers.get(header::IF_MATCH)
        && let Ok(s) = if_match.to_str()
        && s != expected
    {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            format!("If-Match {s:?} doesn't match current ETag {expected:?}"),
        ));
    }

    let suggested_filename = format!("{name}-{version}");
    let range = parse_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        total_size,
    );

    match range {
        RangeResult::Invalid => {
            let body =
                format!("Range header invalid or out of bounds for object size {total_size}\n");
            Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total_size}"))],
                body,
            )
                .into_response())
        }
        RangeResult::None => {
            let mut resp = (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (header::CONTENT_LENGTH, total_size.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
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
        RangeResult::Valid(ByteRange { start, end }) => {
            let end_inclusive = end.unwrap_or(total_size - 1);
            let body_len = end_inclusive - start + 1;

            // Same async-nats chunk-level read caveat as
            // app_packages::download (kanadehq/kanade#209). The
            // size cap on this bucket (4 MB) makes the prefix-skip
            // cost much smaller than the installer bucket, but
            // the underlying limitation is identical.
            if start > 0 {
                let mut taker = (&mut obj).take(start);
                tokio::io::copy(&mut taker, &mut tokio::io::sink())
                    .await
                    .map_err(|e| {
                        warn!(error = %e, %key, start, "range skip");
                        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                    })?;
            }
            let limited = obj.take(body_len);

            let mut resp = (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (header::CONTENT_LENGTH, body_len.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end_inclusive}/{total_size}"),
                    ),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{suggested_filename}\""),
                    ),
                ],
                Body::from_stream(ReaderStream::new(limited)),
            )
                .into_response();
            if let Some(etag) = etag
                && let Ok(v) = etag.parse()
            {
                resp.headers_mut().insert(header::ETAG, v);
            }
            Ok(resp)
        }
    }
}

// ─── DELETE /api/script-objects/{name}/{version} ─────────────────────

pub async fn delete_object(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;

    let key = object_key(&name, &version);
    let store = state
        .jetstream
        .get_object_store(OBJECT_SCRIPTS)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    store.delete(key.as_str()).await.map_err(|e| {
        warn!(error = %e, %key, "object_store.delete");
        let msg = e.to_string();
        if msg.contains("not found") || msg.contains("no objects") {
            (
                StatusCode::NOT_FOUND,
                format!("script object '{name}/{version}' not in Object Store"),
            )
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    })?;
    info!(name, version, "script_objects: deleted");

    // #1216: write-through so the SPA's immediate post-delete refetch
    // no longer lists the object (watcher would heal, but slower).
    if let Err(e) =
        crate::projector::object_meta::delete_key(&state.pool, OBJECT_SCRIPTS, &key).await
    {
        warn!(error = %e, %key, "object_meta write-through failed (watcher will heal)");
    }

    audit::record(
        &state.nats,
        "operator",
        "script_object_delete",
        Some(&key),
        Some(&caller),
        serde_json::json!({
            "name": name,
            "version": version,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_combines_name_and_version_with_slash() {
        assert_eq!(
            object_key("inventory-os", "2025.03"),
            "inventory-os/2025.03"
        );
        assert_eq!(
            object_key("ad-hoc-cleanup", "7c6f9d4a"),
            "ad-hoc-cleanup/7c6f9d4a"
        );
    }

    #[test]
    fn validate_segment_rejects_empty_and_slash() {
        assert!(validate_segment("name", "inventory-os").is_ok());
        assert!(validate_segment("version", "2025.03").is_ok());

        let err = validate_segment("name", "").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("non-empty"));

        let err = validate_segment("version", "1/2").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("'/'"));
    }

    #[test]
    fn validate_segment_allows_common_manifest_id_shapes() {
        assert!(validate_segment("name", "inventory-os").is_ok());
        assert!(validate_segment("name", "manifest_with_underscore").is_ok());
        assert!(validate_segment("version", "0.41.0").is_ok());
        assert!(validate_segment("version", "0.41.0-beta.2").is_ok());
        // Git-sha-like is the common case for ad-hoc scripts that
        // don't carry a semantic version.
        assert!(validate_segment("version", "7c6f9d4a").is_ok());
    }

    #[test]
    fn validate_segment_rejects_non_ascii() {
        let err = validate_segment("name", "勤怠スクリプト").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("non-ASCII"));
    }

    #[test]
    fn validate_segment_rejects_control_characters() {
        let err = validate_segment("name", "ad\nhoc").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("control"));
    }

    #[test]
    fn validate_segment_rejects_quote_and_backslash() {
        let err = validate_segment("name", "a\"b").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let err = validate_segment("name", "a\\b").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    // ---- parse_range ----

    #[test]
    fn parse_range_none_when_header_missing() {
        assert_eq!(parse_range(None, 100), RangeResult::None);
    }

    #[test]
    fn parse_range_open_ended_resume() {
        assert_eq!(
            parse_range(Some("bytes=50-"), 100),
            RangeResult::Valid(ByteRange {
                start: 50,
                end: None
            }),
        );
    }

    #[test]
    fn parse_range_closed_interval() {
        assert_eq!(
            parse_range(Some("bytes=10-99"), 100),
            RangeResult::Valid(ByteRange {
                start: 10,
                end: Some(99),
            }),
        );
    }

    #[test]
    fn parse_range_rejects_missing_bytes_prefix() {
        assert_eq!(parse_range(Some("0-10"), 100), RangeResult::Invalid);
    }

    #[test]
    fn parse_range_rejects_suffix_form_today() {
        assert_eq!(parse_range(Some("bytes=-50"), 100), RangeResult::Invalid);
    }

    #[test]
    fn parse_range_rejects_start_past_eof() {
        assert_eq!(parse_range(Some("bytes=100-"), 100), RangeResult::Invalid);
        assert_eq!(
            parse_range(Some("bytes=200-300"), 100),
            RangeResult::Invalid
        );
    }

    #[test]
    fn parse_range_rejects_end_past_eof() {
        assert_eq!(parse_range(Some("bytes=50-100"), 100), RangeResult::Invalid);
    }

    #[test]
    fn parse_range_rejects_end_before_start() {
        assert_eq!(parse_range(Some("bytes=50-40"), 100), RangeResult::Invalid);
    }

    #[test]
    fn parse_range_rejects_garbage_numbers() {
        assert_eq!(
            parse_range(Some("bytes=abc-def"), 100),
            RangeResult::Invalid
        );
        assert_eq!(parse_range(Some("bytes=10-xyz"), 100), RangeResult::Invalid);
    }

    #[test]
    fn parse_range_zero_offset_is_valid_full_resume() {
        assert_eq!(
            parse_range(Some("bytes=0-"), 100),
            RangeResult::Valid(ByteRange {
                start: 0,
                end: None
            }),
        );
    }
}
