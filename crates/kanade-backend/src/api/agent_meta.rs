//! Per-PC operator metadata (`agent_meta` KV bucket).
//!
//!   GET /api/agents/{pc_id}/meta  (viewer+) -> AgentMeta
//!   PUT /api/agents/{pc_id}/meta  (operator) replace the whole set
//!
//! Parallel to `agent_groups` membership: per-PC, operator-managed,
//! stored straight in JetStream KV (no SQLite projection — nothing on the
//! agent side reads it; the backend reads it on demand for the SPA). The
//! PUT re-normalises (trim / drop empty keys / dedup by key) via
//! [`AgentMeta::new`] so the stored JSON is stable regardless of input.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use tracing::{info, warn};

use kanade_shared::kv::BUCKET_AGENT_META;
use kanade_shared::wire::AgentMeta;

use super::AppState;

/// `GET /api/agents/{pc_id}/meta` — the PC's key/value attributes (an
/// empty set when none are set).
pub async fn get_meta(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<Json<AgentMeta>, (StatusCode, String)> {
    let kv = open_bucket(&state).await?;
    Ok(Json(read_or_default(&kv, &pc_id).await?))
}

/// `PUT /api/agents/{pc_id}/meta` — replace the PC's whole attribute set.
/// Normalises (trim / drop empty keys / dedup by key, last-value-wins) so
/// two operators entering the same logical set store identical JSON.
pub async fn put_meta(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
    Json(payload): Json<AgentMeta>,
) -> Result<Json<AgentMeta>, (StatusCode, String)> {
    let normalised = AgentMeta::new(payload.entries);
    let kv = open_bucket(&state).await?;
    let bytes = serde_json::to_vec(&normalised).map_err(|e| {
        warn!(error = %e, pc_id = %pc_id, "encode agent_meta");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode agent_meta for {pc_id}: {e}"),
        )
    })?;
    kv.put(pc_id.as_str(), bytes.into()).await.map_err(|e| {
        warn!(error = %e, pc_id = %pc_id, "write agent_meta");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write agent_meta for {pc_id}: {e}"),
        )
    })?;
    info!(pc_id = %pc_id, count = normalised.entries.len(), "agent_meta replaced");
    Ok(Json(normalised))
}

async fn open_bucket(
    state: &AppState,
) -> Result<async_nats::jetstream::kv::Store, (StatusCode, String)> {
    state
        .jetstream
        .get_key_value(BUCKET_AGENT_META)
        .await
        .map_err(|e| {
            warn!(error = %e, bucket = BUCKET_AGENT_META, "open agent_meta KV bucket");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("agent_meta KV bucket unavailable: {e}"),
            )
        })
}

async fn read_or_default(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
) -> Result<AgentMeta, (StatusCode, String)> {
    match kv.get(pc_id).await {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).map_err(|e| {
            warn!(error = %e, pc_id, "decode agent_meta");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decode agent_meta for {pc_id}: {e}"),
            )
        }),
        Ok(None) => Ok(AgentMeta::default()),
        Err(e) => {
            warn!(error = %e, pc_id, "read agent_meta");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read agent_meta for {pc_id}: {e}"),
            ))
        }
    }
}
