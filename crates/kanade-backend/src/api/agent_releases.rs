//! HTTP surface for the staged self-update flow.
//!
//! * `POST /api/agents/publish` — multipart upload (`file` =
//!   binary, `version` = label) → puts the bytes in the
//!   `agent_releases` Object Store. Mirrors `kanade agent
//!   publish` on the CLI side; the SPA's Rollout page wires a
//!   file picker to this endpoint.
//! * `GET  /api/agents/releases` — list every version present in
//!   the Object Store. Used by the Web UI's rollout picker.
//! * `POST /api/agents/rollout` — flip `target_version` (and
//!   optionally `target_version_jitter`) on one scope of the
//!   layered `agent_config` bucket. Mirrors the CLI's `rollout`
//!   subcommand.

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use futures::StreamExt;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, KEY_AGENT_CONFIG_GLOBAL, OBJECT_AGENT_RELEASES, agent_config_group_key,
    agent_config_pc_key,
};
use kanade_shared::wire::ConfigScope;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::AppState;

// ─── POST /api/agents/publish ────────────────────────────────────────

#[derive(Serialize)]
pub struct PublishResponse {
    pub version: String,
    pub size: u64,
    pub digest: Option<String>,
}

pub async fn publish(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<PublishResponse>, (StatusCode, String)> {
    let mut version: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("read multipart field: {e}"),
        )
    })? {
        match field.name().unwrap_or("") {
            "version" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("read version field: {e}")))?;
                version = Some(text.trim().to_owned());
            }
            "file" => {
                let buf = field
                    .bytes()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("read file field: {e}")))?;
                bytes = Some(buf.to_vec());
            }
            other => {
                warn!(field = other, "publish: ignoring unknown multipart field");
            }
        }
    }

    let version = version
        .filter(|v| !v.is_empty())
        .ok_or((StatusCode::BAD_REQUEST, "missing 'version' field".into()))?;
    let bytes = bytes.ok_or((StatusCode::BAD_REQUEST, "missing 'file' field".into()))?;
    if bytes.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "'file' field is empty".into()));
    }

    let size = bytes.len() as u64;
    info!(version, size, "publish: uploading new agent binary");

    let store = state
        .jetstream
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .map_err(|e| {
            warn!(error = %e, "get_object_store agent_releases");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Object Store '{OBJECT_AGENT_RELEASES}' missing — run `kanade jetstream setup`"
                ),
            )
        })?;
    let mut cursor = std::io::Cursor::new(bytes);
    let meta = store
        .put(version.as_str(), &mut cursor)
        .await
        .map_err(|e| {
            warn!(error = %e, "object_store.put");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    info!(version, digest = ?meta.digest, "publish: agent binary uploaded");

    Ok(Json(PublishResponse {
        version,
        size,
        digest: meta.digest,
    }))
}

// ─── GET /api/agents/releases ────────────────────────────────────────

#[derive(Serialize)]
pub struct ReleaseRow {
    pub version: String,
    pub size: u64,
    pub digest: Option<String>,
    /// RFC3339 timestamp from the Object Store metadata. The
    /// underlying async-nats type is `time::OffsetDateTime`; we
    /// stringify so the SPA can parse it with `Date.parse` and
    /// kanade-shared doesn't need a time-crate dep.
    pub modified: Option<String>,
}

pub async fn list_releases(
    State(state): State<AppState>,
) -> Result<Json<Vec<ReleaseRow>>, (StatusCode, String)> {
    let store = state
        .jetstream
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .map_err(|e| {
            warn!(error = %e, "get_object_store agent_releases");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Object Store '{OBJECT_AGENT_RELEASES}' missing — run `kanade jetstream setup`"
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
        // async-nats' ObjectMeta.modified is a time::OffsetDateTime;
        // convert to chrono via Unix nanos so the wire shape matches
        // every other `*_at` field on the backend.
        let modified = meta.modified.and_then(|t| {
            let nanos = t.unix_timestamp_nanos();
            let secs = (nanos.div_euclid(1_000_000_000)) as i64;
            let nsec = (nanos.rem_euclid(1_000_000_000)) as u32;
            chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nsec).map(|d| d.to_rfc3339())
        });
        rows.push(ReleaseRow {
            version: meta.name,
            size: meta.size as u64,
            digest: meta.digest,
            modified,
        });
    }
    rows.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(Json(rows))
}

// ─── POST /api/agents/rollout ────────────────────────────────────────

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum RolloutScope {
    Global,
    Group(String),
    Pc(String),
}

#[derive(Deserialize, Debug)]
pub struct RolloutBody {
    pub version: String,
    pub scope: RolloutScope,
    /// Optional `target_version_jitter` override on the same scope
    /// (humantime, e.g. `"30m"`). Omit to leave the existing value
    /// alone.
    #[serde(default)]
    pub jitter: Option<String>,
}

#[derive(Serialize)]
pub struct RolloutResponse {
    pub version: String,
    pub scope_key: String,
    pub scope_label: String,
    pub jitter: Option<String>,
}

pub async fn rollout(
    State(state): State<AppState>,
    Json(body): Json<RolloutBody>,
) -> Result<Json<RolloutResponse>, (StatusCode, String)> {
    let (key, label) = match &body.scope {
        RolloutScope::Global => (KEY_AGENT_CONFIG_GLOBAL.to_string(), "global".to_string()),
        RolloutScope::Group(g) => (agent_config_group_key(g), format!("group:{g}")),
        RolloutScope::Pc(p) => (agent_config_pc_key(p), format!("pc:{p}")),
    };

    // Fail-fast on a version that doesn't have a binary uploaded yet.
    let store = state
        .jetstream
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    if store.info(&body.version).await.is_err() {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "version '{}' not found in {OBJECT_AGENT_RELEASES} — run `kanade agent publish` first",
                body.version
            ),
        ));
    }

    let kv = state
        .jetstream
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    let mut scope = match kv.get(&key).await.map_err(|e| {
        warn!(error = %e, %key, "kv.get scope");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })? {
        Some(b) => serde_json::from_slice::<ConfigScope>(&b).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decode existing {BUCKET_AGENT_CONFIG}.{key}: {e}"),
            )
        })?,
        None => ConfigScope::default(),
    };
    scope.target_version = Some(body.version.clone());
    if let Some(j) = body.jitter.as_deref() {
        scope.target_version_jitter = Some(j.to_owned());
    }
    let payload = serde_json::to_vec(&scope).map_err(|e| {
        warn!(error = %e, "encode ConfigScope");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    kv.put(key.as_str(), payload.into()).await.map_err(|e| {
        warn!(error = %e, %key, "kv.put scope");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    info!(
        scope = %label,
        version = %body.version,
        jitter = ?body.jitter,
        "rollout: target_version flipped via HTTP",
    );

    Ok(Json(RolloutResponse {
        version: body.version,
        scope_key: key,
        scope_label: label,
        jitter: body.jitter,
    }))
}
