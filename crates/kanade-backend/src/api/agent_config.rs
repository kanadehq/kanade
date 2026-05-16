//! Admin API for the layered `agent_config` KV bucket (Sprint 6).
//!
//! Routes:
//!   GET    /api/config                        -> global ConfigScope
//!   PUT    /api/config                        (replace global scope)
//!   GET    /api/groups/{name}/config          -> group ConfigScope
//!   PUT    /api/groups/{name}/config          (replace group scope)
//!   DELETE /api/groups/{name}/config          (drop the row)
//!   GET    /api/pcs/{pc_id}/config            -> pc ConfigScope
//!   PUT    /api/pcs/{pc_id}/config            (replace pc scope)
//!   DELETE /api/pcs/{pc_id}/config            (drop the row)
//!   GET    /api/agents/{pc_id}/effective_config
//!     -> the resolved EffectiveConfig + any ResolutionWarnings the
//!        resolver emitted. Read-only convenience for debugging
//!        "why is this PC running version X?"
//!
//! All handlers go straight at the JetStream KV bucket — no SQLite
//! projection. The agent-side config_supervisor watches the same
//! bucket and reconciles within one NATS round-trip of the write.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::Serialize;
use tracing::{info, warn};

use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, KEY_AGENT_CONFIG_GLOBAL, agent_config_group_key,
    agent_config_pc_key, parse_agent_config_group_key,
};
use kanade_shared::wire::{AgentGroups, ConfigScope, EffectiveConfig, ResolutionWarning, resolve};

use super::AppState;

// -------- global scope --------

pub async fn get_global(
    State(state): State<AppState>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    Ok(Json(
        read_scope_or_default(&kv, KEY_AGENT_CONFIG_GLOBAL).await?,
    ))
}

pub async fn put_global(
    State(state): State<AppState>,
    Json(scope): Json<ConfigScope>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    write_scope(&kv, KEY_AGENT_CONFIG_GLOBAL, &scope).await?;
    info!(scope = ?scope, "agent_config.global replaced");
    Ok(Json(scope))
}

// -------- per-group scope --------

pub async fn get_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    Ok(Json(
        read_scope_or_default(&kv, &agent_config_group_key(&name)).await?,
    ))
}

pub async fn put_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(scope): Json<ConfigScope>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_group_key(&name);
    write_scope(&kv, &key, &scope).await?;
    info!(group = %name, scope = ?scope, "agent_config.groups.<name> replaced");
    Ok(Json(scope))
}

pub async fn delete_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_group_key(&name);
    delete_key(&kv, &key).await?;
    info!(group = %name, "agent_config.groups.<name> deleted");
    Ok(StatusCode::NO_CONTENT)
}

// -------- per-pc scope --------

pub async fn get_pc(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    Ok(Json(
        read_scope_or_default(&kv, &agent_config_pc_key(&pc_id)).await?,
    ))
}

pub async fn put_pc(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
    Json(scope): Json<ConfigScope>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_pc_key(&pc_id);
    write_scope(&kv, &key, &scope).await?;
    info!(pc_id = %pc_id, scope = ?scope, "agent_config.pcs.<pc_id> replaced");
    Ok(Json(scope))
}

pub async fn delete_pc(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_pc_key(&pc_id);
    delete_key(&kv, &key).await?;
    info!(pc_id = %pc_id, "agent_config.pcs.<pc_id> deleted");
    Ok(StatusCode::NO_CONTENT)
}

// -------- resolved view --------

#[derive(Serialize)]
pub struct EffectiveConfigResponse {
    pub pc_id: String,
    pub effective: EffectiveConfig,
    pub warnings: Vec<String>,
}

/// Return the resolved EffectiveConfig for `pc_id` — the same
/// computation the agent's config_supervisor runs locally — plus
/// any ResolutionWarning the resolver emitted (rendered as strings
/// so the operator can read them straight out of curl).
pub async fn effective(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<Json<EffectiveConfigResponse>, (StatusCode, String)> {
    let cfg_kv = open_cfg(&state).await?;
    let groups_kv = open_groups(&state).await?;

    // Walk every key in agent_config so we can build the same view
    // the agent would, minus the watch loop.
    let global_scope = read_optional_scope(&cfg_kv, KEY_AGENT_CONFIG_GLOBAL).await?;
    let pc_scope = read_optional_scope(&cfg_kv, &agent_config_pc_key(&pc_id)).await?;

    let mut group_scopes: BTreeMap<String, ConfigScope> = BTreeMap::new();
    match cfg_kv.keys().await {
        Ok(mut keys) => {
            while let Some(k) = keys.next().await {
                let key = match k {
                    Ok(k) => k,
                    Err(e) => {
                        warn!(error = %e, "agent_config keys()");
                        continue;
                    }
                };
                if let Some(group) = parse_agent_config_group_key(&key)
                    && let Ok(Some(bytes)) = cfg_kv.get(&key).await
                    && let Ok(scope) = serde_json::from_slice::<ConfigScope>(&bytes)
                {
                    group_scopes.insert(group.to_string(), scope);
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "agent_config keys() for effective");
        }
    }

    let my_groups: Vec<String> = match groups_kv.get(&pc_id).await {
        Ok(Some(bytes)) => serde_json::from_slice::<AgentGroups>(&bytes)
            .map(|g| g.groups)
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    let (effective, warns) = resolve(
        global_scope.as_ref(),
        &group_scopes,
        pc_scope.as_ref(),
        &my_groups,
    );

    Ok(Json(EffectiveConfigResponse {
        pc_id,
        effective,
        warnings: warns.into_iter().map(render_warning).collect(),
    }))
}

fn render_warning(w: ResolutionWarning) -> String {
    match w {
        ResolutionWarning::MultiGroupConflict { field, groups } => format!(
            "multi-group conflict on `{field}` — set by [{}]; alphabetical last wins (=> {})",
            groups.join(", "),
            groups.last().map(String::as_str).unwrap_or("<none>"),
        ),
    }
}

// -------- helpers --------

async fn open_cfg(
    state: &AppState,
) -> Result<async_nats::jetstream::kv::Store, (StatusCode, String)> {
    state
        .jetstream
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .map_err(|e| {
            warn!(error = %e, bucket = BUCKET_AGENT_CONFIG, "open agent_config KV bucket");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("agent_config KV bucket unavailable: {e}"),
            )
        })
}

async fn open_groups(
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

async fn read_scope_or_default(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
) -> Result<ConfigScope, (StatusCode, String)> {
    match read_optional_scope(kv, key).await? {
        Some(s) => Ok(s),
        None => Ok(ConfigScope::default()),
    }
}

async fn read_optional_scope(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
) -> Result<Option<ConfigScope>, (StatusCode, String)> {
    match kv.get(key).await {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            warn!(error = %e, key, "decode ConfigScope");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decode ConfigScope at {key}: {e}"),
            )
        }),
        Ok(None) => Ok(None),
        Err(e) => {
            warn!(error = %e, key, "read ConfigScope");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read ConfigScope at {key}: {e}"),
            ))
        }
    }
}

async fn write_scope(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
    scope: &ConfigScope,
) -> Result<(), (StatusCode, String)> {
    let bytes = serde_json::to_vec(scope).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode ConfigScope: {e}"),
        )
    })?;
    kv.put(key, bytes.into()).await.map_err(|e| {
        warn!(error = %e, key, "write ConfigScope");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write ConfigScope at {key}: {e}"),
        )
    })?;
    Ok(())
}

async fn delete_key(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
) -> Result<(), (StatusCode, String)> {
    kv.delete(key).await.map_err(|e| {
        warn!(error = %e, key, "delete KV key");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete KV key {key}: {e}"),
        )
    })?;
    Ok(())
}
