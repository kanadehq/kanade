//! Admin API for the `agent_groups` KV bucket (Sprint 5).
//!
//! Routes (all read + write straight through the JetStream KV
//! bucket — no SQLite projection yet, since membership is naturally
//! read by the agent KV-watch rather than the operator HTTP):
//!
//!   GET    /api/agents/{pc_id}/groups          -> AgentGroups
//!   PUT    /api/agents/{pc_id}/groups          (replace whole list)
//!   POST   /api/agents/{pc_id}/groups          (add one group)
//!   DELETE /api/agents/{pc_id}/groups/{group}  (remove one group)
//!
//! All write handlers re-normalise the value (sort + dedup) before
//! storing, so the KV row is bit-identical regardless of operator
//! ordering or duplicate input.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use tracing::{info, warn};

use kanade_shared::kv::BUCKET_AGENT_GROUPS;
use kanade_shared::wire::AgentGroups;

use super::AppState;

pub async fn list_groups(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<Json<AgentGroups>, (StatusCode, String)> {
    let kv = open_bucket(&state).await?;
    Ok(Json(read_or_default(&kv, &pc_id).await?))
}

pub async fn set_groups(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
    Json(payload): Json<AgentGroups>,
) -> Result<Json<AgentGroups>, (StatusCode, String)> {
    let normalised = AgentGroups::new(payload.groups);
    let kv = open_bucket(&state).await?;
    write(&kv, &pc_id, &normalised).await?;
    info!(pc_id = %pc_id, groups = ?normalised.groups, "agent_groups replaced");
    Ok(Json(normalised))
}

#[derive(Deserialize)]
pub struct AddGroupBody {
    pub group: String,
}

pub async fn add_group(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
    Json(body): Json<AddGroupBody>,
) -> Result<Json<AgentGroups>, (StatusCode, String)> {
    if body.group.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "group name must not be empty".into(),
        ));
    }
    let kv = open_bucket(&state).await?;
    let mut current = read_or_default(&kv, &pc_id).await?;
    if current.insert(&body.group) {
        write(&kv, &pc_id, &current).await?;
        info!(pc_id = %pc_id, group = %body.group, "agent_group added");
    }
    Ok(Json(current))
}

pub async fn remove_group(
    State(state): State<AppState>,
    Path((pc_id, group)): Path<(String, String)>,
) -> Result<Json<AgentGroups>, (StatusCode, String)> {
    let kv = open_bucket(&state).await?;
    let mut current = read_or_default(&kv, &pc_id).await?;
    if current.remove(&group) {
        write(&kv, &pc_id, &current).await?;
        info!(pc_id = %pc_id, group = %group, "agent_group removed");
    }
    Ok(Json(current))
}

async fn open_bucket(
    state: &AppState,
) -> Result<async_nats::jetstream::kv::Store, (StatusCode, String)> {
    state
        .jetstream
        .get_key_value(BUCKET_AGENT_GROUPS)
        .await
        .map_err(|e| {
            warn!(error = %e, bucket = BUCKET_AGENT_GROUPS, "open agent_groups KV bucket");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("agent_groups KV bucket unavailable: {e}"),
            )
        })
}

async fn read_or_default(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
) -> Result<AgentGroups, (StatusCode, String)> {
    match kv.get(pc_id).await {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).map_err(|e| {
            warn!(error = %e, pc_id, "decode agent_groups");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decode agent_groups for {pc_id}: {e}"),
            )
        }),
        Ok(None) => Ok(AgentGroups::default()),
        Err(e) => {
            warn!(error = %e, pc_id, "read agent_groups");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read agent_groups for {pc_id}: {e}"),
            ))
        }
    }
}

async fn write(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
    groups: &AgentGroups,
) -> Result<(), (StatusCode, String)> {
    let bytes = serde_json::to_vec(groups).map_err(|e| {
        warn!(error = %e, pc_id, "encode agent_groups");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode agent_groups for {pc_id}: {e}"),
        )
    })?;
    kv.put(pc_id, bytes.into()).await.map_err(|e| {
        warn!(error = %e, pc_id, "write agent_groups");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write agent_groups for {pc_id}: {e}"),
        )
    })?;
    Ok(())
}
