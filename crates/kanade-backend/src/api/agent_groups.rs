//! Admin API for the `agent_groups` KV bucket (Sprint 5).
//!
//! Routes (all read + write straight through the JetStream KV
//! bucket — no SQLite projection yet, since membership is naturally
//! read by the agent KV-watch rather than the operator HTTP):
//!
//!   GET    /api/groups                         -> GroupsOverview (all
//!          groups with member pc_ids — the SPA Groups page driver)
//!   GET    /api/agents/{pc_id}/groups          -> AgentGroups
//!   PUT    /api/agents/{pc_id}/groups          (replace whole list)
//!   POST   /api/agents/{pc_id}/groups          (add one group)
//!   DELETE /api/agents/{pc_id}/groups/{group}  (remove one group)
//!
//! All write handlers re-normalise the value (sort + dedup) before
//! storing, so the KV row is bit-identical regardless of operator
//! ordering or duplicate input.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use kanade_shared::kv::{BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, parse_agent_config_group_key};
use kanade_shared::wire::AgentGroups;

use super::AppState;

/// One row in the group-centric overview: the inverse view of the
/// per-PC `agent_groups` KV rows, plus whether a `groups.<name>`
/// config override exists (a group can exist via config alone, with
/// no members yet — and vice versa).
#[derive(Serialize)]
pub struct GroupSummary {
    pub name: String,
    /// Sorted member pc_ids.
    pub members: Vec<String>,
    pub has_config: bool,
}

#[derive(Serialize)]
pub struct GroupsOverview {
    pub groups: Vec<GroupSummary>,
}

/// GET /api/groups — aggregate every group across the fleet. Same
/// two-pass union as `kanade group list` in the CLI: membership from
/// the `agent_groups` bucket ∪ `groups.<name>` keys in `agent_config`.
pub async fn list_all_groups(
    State(state): State<AppState>,
) -> Result<Json<GroupsOverview>, (StatusCode, String)> {
    // Pass 1 — membership: walk agent_groups, invert pc -> groups
    // into group -> [pc_ids].
    let kv = open_bucket(&state).await?;
    let mut pc_ids: Vec<String> = Vec::new();
    match kv.keys().await {
        Ok(mut keys) => {
            while let Some(k) = keys.next().await {
                match k {
                    Ok(k) => pc_ids.push(k),
                    Err(e) => warn!(error = %e, "agent_groups keys()"),
                }
            }
        }
        Err(e) => {
            // Empty bucket on a fresh broker fails keys() on some
            // async-nats versions — degrade to "no members".
            warn!(error = %e, "agent_groups keys() for overview");
        }
    }

    // Fetch every per-PC row concurrently (bounded) instead of one
    // round-trip at a time — the dominant cost on a large fleet.
    const READ_CONCURRENCY: usize = 16;
    let rows: Vec<(String, AgentGroups)> = futures::stream::iter(pc_ids)
        .map(|pc_id| {
            let kv = kv.clone();
            async move {
                let g = read_or_default(&kv, &pc_id).await?;
                Ok::<_, (StatusCode, String)>((pc_id, g))
            }
        })
        .buffer_unordered(READ_CONCURRENCY)
        .try_collect()
        .await?;

    let mut by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (pc_id, g) in rows {
        for name in g.groups {
            by_group.entry(name).or_default().push(pc_id.clone());
        }
    }

    // Pass 2 — config-only groups: scan agent_config for
    // `groups.<name>` keys so a group whose only trace is a config
    // override still gets a row.
    let mut with_config: BTreeSet<String> = BTreeSet::new();
    if let Ok(cfg_kv) = state.jetstream.get_key_value(BUCKET_AGENT_CONFIG).await
        && let Ok(mut cfg_keys) = cfg_kv.keys().await
    {
        while let Some(k) = cfg_keys.next().await {
            if let Ok(key) = k
                && let Some(name) = parse_agent_config_group_key(&key)
            {
                with_config.insert(name.to_string());
            }
        }
    }

    // Union: every name that appears in either side gets a row.
    let mut all_names: BTreeSet<String> = by_group.keys().cloned().collect();
    all_names.extend(with_config.iter().cloned());

    let groups = all_names
        .into_iter()
        .map(|name| {
            let mut members = by_group.remove(&name).unwrap_or_default();
            members.sort();
            GroupSummary {
                has_config: with_config.contains(&name),
                name,
                members,
            }
        })
        .collect();

    Ok(Json(GroupsOverview { groups }))
}

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
    // #505: CAS read-modify-write — a blind get→put here raced a
    // concurrent add/remove for the same PC and silently dropped
    // one side's change.
    let mut changed = false;
    let current = kanade_shared::kv_cas::read_modify_write(&kv, &pc_id, |g: &mut AgentGroups| {
        changed = g.insert(&body.group);
        changed
    })
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id, "add agent_group");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("add group for {pc_id}: {e:#}"),
        )
    })?;
    if changed {
        info!(pc_id = %pc_id, group = %body.group, "agent_group added");
    }
    Ok(Json(current))
}

pub async fn remove_group(
    State(state): State<AppState>,
    Path((pc_id, group)): Path<(String, String)>,
) -> Result<Json<AgentGroups>, (StatusCode, String)> {
    let kv = open_bucket(&state).await?;
    let mut changed = false;
    let current = kanade_shared::kv_cas::read_modify_write(&kv, &pc_id, |g: &mut AgentGroups| {
        changed = g.remove(&group);
        changed
    })
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id, "remove agent_group");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("remove group for {pc_id}: {e:#}"),
        )
    })?;
    if changed {
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
