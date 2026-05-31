//! HTTP surface for the generic app-package distribution flow.
//!
//! See `kanade-shared::kv::OBJECT_APP_PACKAGES` for the bucket-
//! level design notes. In short: operator-curated user-space
//! binaries (kanade-client, Webex, Teams, custom installers,
//! …) keyed by `<name>/<version>`. Distinct from the agent's
//! self-update path (`agent_releases`) so the two lifecycles
//! don't share lifetimes / audit channels.
//!
//! Endpoints (all under `/api/app-packages`):
//!
//! - `GET    /api/app-packages` — list every package + version.
//! - `POST   /api/app-packages/{name}/{version}` — multipart
//!   upload (`file = <bytes>`), replaces any existing object at
//!   the same key.
//! - `GET    /api/app-packages/{name}/{version}` — stream the
//!   stored bytes. Used by kitting / install scripts via plain
//!   HTTP.
//! - `DELETE /api/app-packages/{name}/{version}` — gc.
//!
//! No auth gate today (the rest of the backend is the same — see
//! the agent_releases module for the same posture).
//!
//! Naming rules: the upload endpoint validates that both `name`
//! and `version` are non-empty and slash-free, matching the
//! Object Store key constraint (a slash in either would create
//! an ambiguous path).

use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt, TryStreamExt};
use kanade_shared::kv::OBJECT_APP_PACKAGES;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio_util::io::{ReaderStream, StreamReader};
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
/// Restrictions:
/// - non-empty
/// - no `/` (would create ambiguous Object Store paths)
/// - ASCII-printable only — rejects control characters
///   (`U+0000..U+001F`, `U+007F`) AND non-ASCII so the
///   `filename=` quoted-string in the download response stays
///   well-formed without RFC 5987 percent-encoding
/// - no `"` / `\` — quoted-string delimiters; escaping them
///   inline is possible but accepting them only forces every
///   downstream tool (URL-bar paste, scripts, audit log) to
///   re-quote, so we just reject at the door
///
/// Common semver / calendar / lib-name forms (`0.41.0`,
/// `2025.03`, `webex-meetings`, `kanade_client`) all pass.
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

// ─── POST /api/app-packages/{name}/{version} ─────────────────────────

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

    // Stream the `file` field straight into `ObjectStore::put`
    // instead of `field.bytes()` which buffers the entire payload
    // in RAM. With the body limit at 8 GB (`APP_PACKAGE_BODY_LIMIT`)
    // and multi-GB ISOs / multi-arch installers in scope, buffering
    // would OOM the backend on the first concurrent upload. The
    // streaming path keeps backend RSS flat at the multipart parser's
    // internal chunk size (~8 KB) regardless of payload size; async-
    // nats re-chunks at ~128 KB per JetStream publish, so the broker
    // sees a stream of small messages too.
    //
    // `field.name()` is available before we touch the body, so we
    // can dispatch on the field name and only consume the `file`
    // field as a stream. Other fields (none in the current contract;
    // operators sometimes attach metadata in scratch tests) get
    // drained via `field.bytes()` into a discarded Vec so the
    // multipart parser advances cleanly to the next field.
    let key = object_key(&name, &version);
    let store = state
        .jetstream
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .map_err(|e| {
            warn!(error = %e, "get_object_store app_packages");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Object Store '{OBJECT_APP_PACKAGES}' missing — run `kanade jetstream setup`"
                ),
            )
        })?;

    let mut meta = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("read multipart field: {e}"),
        )
    })? {
        // Snapshot the name before consuming `field` — `field.name()`
        // borrows from `field`, and the `file` arm needs to take
        // ownership for the streaming `put` call below.
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "file" => {
                if meta.is_some() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "multipart body has more than one 'file' field".into(),
                    ));
                }
                info!(name, version, key, "app_packages: streaming upload");
                // axum's `Field` implements `Stream<Item = Result<Bytes,
                // MultipartError>>`; map the error so `StreamReader`'s
                // `AsyncRead` impl can surface it as `io::Error`.
                let body_stream = field.map_err(std::io::Error::other);
                let mut reader = StreamReader::new(body_stream);
                let m = store.put(key.as_str(), &mut reader).await.map_err(|e| {
                    warn!(error = %e, %key, "object_store.put");
                    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                })?;
                meta = Some(m);
            }
            _ => {
                warn!(
                    field = field_name,
                    "app_packages.publish: draining unknown multipart field"
                );
                // Consume the body so the parser advances. Cheap on
                // the small-field path (operators only send `file`
                // today) and bounded by the request body limit.
                let _ = field.bytes().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("drain field {field_name:?}: {e}"),
                    )
                })?;
            }
        }
    }
    let meta = meta.ok_or((StatusCode::BAD_REQUEST, "missing 'file' field".into()))?;
    let size = meta.size as u64;
    if size == 0 {
        return Err((StatusCode::BAD_REQUEST, "'file' field is empty".into()));
    }
    info!(name, version, size, digest = ?meta.digest, "app_packages: uploaded");

    audit::record(
        &state.nats,
        "operator",
        "app_package_publish",
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

// ─── GET /api/app-packages ───────────────────────────────────────────

#[derive(Serialize)]
pub struct PackageRow {
    pub name: String,
    pub version: String,
    pub size: u64,
    pub digest: Option<String>,
    /// RFC3339 timestamp from the Object Store metadata.
    pub modified: Option<String>,
}

pub async fn list_packages(
    State(state): State<AppState>,
) -> Result<Json<Vec<PackageRow>>, (StatusCode, String)> {
    let store = state
        .jetstream
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .map_err(|e| {
            warn!(error = %e, "get_object_store app_packages");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Object Store '{OBJECT_APP_PACKAGES}' missing — run `kanade jetstream setup`"
                ),
            )
        })?;
    let mut list = store.list().await.map_err(|e| {
        warn!(error = %e, "object_store.list");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let mut rows = Vec::new();
    while let Some(item) = list.next().await {
        // Propagate stream errors instead of silently truncating
        // — a partial list returned as `200 OK` would lie to the
        // operator about what's in the bucket.
        let meta = item.map_err(|e| {
            warn!(error = %e, "app_packages.list: object metadata stream error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list app packages: {e}"),
            )
        })?;
        // Object key is `<name>/<version>` — split on the LAST
        // slash so future packages with a slash-containing name
        // wouldn't trip the parser (defensive; current
        // `validate_segment` forbids slashes either way).
        let (name, version) = match meta.name.rsplit_once('/') {
            Some((n, v)) => (n.to_string(), v.to_string()),
            None => {
                warn!(key = %meta.name, "app_packages.list: object key has no '/' — skipping");
                continue;
            }
        };
        // `time::OffsetDateTime` exposes the seconds + sub-second
        // components separately, so we skip the nanos-divide
        // jig the agent_releases module currently does (kept
        // there for back-compat; sibling-cleanup is a separate
        // PR if it gets noticed).
        let modified = meta
            .modified
            .and_then(|t| chrono::DateTime::from_timestamp(t.unix_timestamp(), t.nanosecond()))
            .map(|d| d.to_rfc3339());
        rows.push(PackageRow {
            name,
            version,
            size: meta.size as u64,
            digest: meta.digest,
            modified,
        });
    }
    // Sort: newest first, then alphabetically within the same
    // modified time (deterministic for tests + matches the SPA's
    // expected most-recent-on-top ordering).
    rows.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.version.cmp(&b.version))
    });
    Ok(Json(rows))
}

// ─── GET /api/app-packages/{name}/{version} ──────────────────────────

/// Parsed `Range:` header. `bytes=N-` (suffix-less open) and
/// `bytes=N-M` (closed interval) supported; multipart ranges and
/// suffix-only `bytes=-N` (last N bytes) are not — operators
/// don't need them, and skipping them keeps the response-builder
/// simple.
#[derive(Debug, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    /// Inclusive end byte; `None` ⇒ "to end of object".
    end: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum RangeResult {
    None,
    Valid(ByteRange),
    Invalid,
}

/// Pull a `bytes=N-[M]` Range out of an optional header value,
/// validated against the object's total size. Returns
/// `RangeResult::Invalid` for malformed / out-of-bounds requests
/// (caller maps to 416 Range Not Satisfiable).
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
    // Reject suffix range `bytes=-N` for now (would need a
    // separate seek strategy, and no operator-facing tool sends
    // it).
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
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let mut obj = match store.get(key.as_str()).await {
        Ok(o) => o,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("no objects") {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("app package '{name}/{version}' not found"),
                ));
            }
            warn!(error = %e, %key, "object_store.get");
            return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
        }
    };

    // Snapshot metadata before consuming the AsyncRead.
    let total_size = obj.info().size as u64;
    let digest = obj.info().digest.clone();

    // `ETag: "<digest>"` — the Object Store stores a content
    // digest per object, so we can hand the client a strong
    // validator for free. Clients that resume across re-uploads
    // can guard with `If-Match` to refuse mid-flight version
    // drift (operator re-uploaded the same name/version with
    // new bytes while a download was paused).
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
            // RFC 7233 §4.4: a 416 response SHOULD include a
            // `Content-Range: bytes */<complete-length>` header
            // so the client knows how to retry with a satisfiable
            // range. Hand-build the Response so we can set both
            // status + header in one shot.
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
            // Full-content response. Stream directly from the
            // Object Store — never buffer the (potentially
            // multi-hundred-MB) payload in RAM.
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

            // Perf note (tracked in yukimemi/kanade#209):
            // async-nats' Object Store doesn't expose chunk-level
            // reads, so resuming a partial download forces the
            // backend to read+discard the prefix from NATS. WAN
            // traffic to the client IS bounded by `body_len`
            // (the actual win), but the backend ↔ broker leg
            // still ships the skipped bytes. Swap to chunk-level
            // read once async-nats publishes a
            // `get_chunks(name, start_chunk)` style API.
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

// ─── DELETE /api/app-packages/{name}/{version} ───────────────────────

pub async fn delete_package(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;

    let key = object_key(&name, &version);
    let store = state
        .jetstream
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    store.delete(key.as_str()).await.map_err(|e| {
        warn!(error = %e, %key, "object_store.delete");
        let msg = e.to_string();
        if msg.contains("not found") || msg.contains("no objects") {
            (
                StatusCode::NOT_FOUND,
                format!("app package '{name}/{version}' not in Object Store"),
            )
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    })?;
    info!(name, version, "app_packages: deleted");

    audit::record(
        &state.nats,
        "operator",
        "app_package_delete",
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
            object_key("kanade-client", "0.41.0"),
            "kanade-client/0.41.0"
        );
        assert_eq!(
            object_key("webex-meetings", "2025.03"),
            "webex-meetings/2025.03"
        );
    }

    #[test]
    fn validate_segment_rejects_empty_and_slash() {
        assert!(validate_segment("name", "kanade-client").is_ok());
        assert!(validate_segment("version", "0.41.0").is_ok());

        let err = validate_segment("name", "").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("non-empty"));

        let err = validate_segment("version", "1/2").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("'/'"));
    }

    #[test]
    fn validate_segment_allows_dotted_versions_and_dashes() {
        // Common semver + calendar forms operators will actually use.
        assert!(validate_segment("version", "0.41.0").is_ok());
        assert!(validate_segment("version", "0.41.0-beta.2").is_ok());
        assert!(validate_segment("version", "2025.03").is_ok());
        assert!(validate_segment("name", "webex_meetings").is_ok());
    }

    #[test]
    fn validate_segment_rejects_non_ascii() {
        // Japanese package names look fine on the SPA but would
        // break `Content-Disposition: filename="…"` without RFC
        // 5987 percent-encoding. Reject at the door so the
        // download header stays simple.
        let err = validate_segment("name", "勤怠アプリ").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("non-ASCII"));
    }

    #[test]
    fn validate_segment_rejects_control_characters() {
        let err = validate_segment("name", "kanade\nclient").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("control"));
    }

    #[test]
    fn validate_segment_rejects_quote_and_backslash() {
        // Either character would break the quoted-string in
        // Content-Disposition without escape gymnastics.
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
        // `bytes=50-` — "give me bytes 50 through end". This is
        // the shape PowerShell `-Resume` sends.
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
        // `bytes=-50` (last 50 bytes) — not implemented; should
        // fall through to 416 so the client retries with a
        // canonical form.
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
        // Total = 100 ⇒ last valid byte is 99.
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
        // Edge: `bytes=0-` is technically valid — equivalent to
        // a full GET but with the Range/206 round-trip. Accept
        // it so clients (PowerShell -Resume on a fresh download)
        // don't choke.
        assert_eq!(
            parse_range(Some("bytes=0-"), 100),
            RangeResult::Valid(ByteRange {
                start: 0,
                end: None
            }),
        );
    }
}
