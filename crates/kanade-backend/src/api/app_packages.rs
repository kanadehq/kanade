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
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use kanade_shared::kv::OBJECT_APP_PACKAGES;
use serde::Serialize;
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
/// segments. Reject empty strings + slashes; everything else
/// (dots, dashes, semver labels) is fine.
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

    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("read multipart field: {e}"),
        )
    })? {
        match field.name().unwrap_or("") {
            "file" => {
                let buf = field
                    .bytes()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("read file field: {e}")))?;
                bytes = Some(buf.to_vec());
            }
            other => {
                warn!(
                    field = other,
                    "app_packages.publish: ignoring unknown multipart field"
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
    info!(name, version, size, key, "app_packages: uploading");

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
    let mut cursor = std::io::Cursor::new(bytes);
    let meta = store.put(key.as_str(), &mut cursor).await.map_err(|e| {
        warn!(error = %e, %key, "object_store.put");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    info!(name, version, digest = ?meta.digest, "app_packages: uploaded");

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
    while let Some(meta) = list.next().await {
        let Ok(meta) = meta else { continue };
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
        let modified = meta.modified.and_then(|t| {
            let nanos = t.unix_timestamp_nanos();
            let secs = (nanos.div_euclid(1_000_000_000)) as i64;
            let nsec = (nanos.rem_euclid(1_000_000_000)) as u32;
            chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nsec).map(|d| d.to_rfc3339())
        });
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

pub async fn download(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
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

    // Read into memory. Future-work for huge installers: stream
    // via `Body::from_stream` once we have something past 64 MB
    // in the bucket. For now the upload cap matches the read
    // strategy.
    let mut buf = Vec::with_capacity(obj.info().size);
    tokio::io::copy(&mut obj, &mut buf).await.map_err(|e| {
        warn!(error = %e, %key, "object copy");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let suggested_filename = format!("{name}-{version}");
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (
                header::CONTENT_DISPOSITION,
                // Hint for browser-driven downloads from the SPA.
                // Programmatic clients (PowerShell `-OutFile`)
                // ignore this and use their own path.
                &format!("attachment; filename=\"{suggested_filename}\""),
            ),
        ],
        Body::from(buf),
    )
        .into_response())
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
}
