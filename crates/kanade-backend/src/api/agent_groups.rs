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

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENT_GROUPS_DERIVED,
    parse_agent_config_group_key,
};
use kanade_shared::wire::AgentGroups;

use super::AppState;

/// Collect every `pc_id -> AgentGroups` row from one membership bucket,
/// best-effort (an unopenable bucket or a per-key read error contributes
/// nothing). Shared by the manual `agent_groups` and materialized
/// `agent_groups_derived` walks.
async fn collect_membership_rows(state: &AppState, bucket: &str) -> Vec<(String, AgentGroups)> {
    let Ok(kv) = state.jetstream.get_key_value(bucket).await else {
        return Vec::new();
    };
    let mut pc_ids: Vec<String> = Vec::new();
    if let Ok(mut keys) = kv.keys().await {
        while let Some(k) = keys.next().await {
            match k {
                Ok(k) => pc_ids.push(k),
                Err(e) => warn!(error = %e, bucket, "membership keys()"),
            }
        }
    }
    // Fetch the per-PC rows concurrently (bounded) — the dominant cost on a
    // large fleet. A per-PC read error degrades that PC to "no groups".
    const READ_CONCURRENCY: usize = 16;
    futures::stream::iter(pc_ids)
        .map(|pc_id| {
            let kv = kv.clone();
            async move {
                let g = read_or_default(&kv, &pc_id).await.ok()?;
                Some((pc_id, g))
            }
        })
        .buffer_unordered(READ_CONCURRENCY)
        .filter_map(|x| async move { x })
        .collect()
        .await
}

/// The fleet's **effective** membership `pc_id -> sorted [group]` — the union of
/// the operator-owned `agent_groups` and the materializer-owned
/// `agent_groups_derived` (#1032①). A PC in a dynamic group via the derived
/// bucket therefore counts in the group overview and the notification audience,
/// exactly as a manually-assigned PC does. PCs with no groups are present with
/// an empty list; callers that want only members filter that out.
async fn effective_membership(state: &AppState) -> HashMap<String, Vec<String>> {
    let mut merged: HashMap<String, BTreeSet<String>> = HashMap::new();
    for bucket in [BUCKET_AGENT_GROUPS, BUCKET_AGENT_GROUPS_DERIVED] {
        for (pc_id, g) in collect_membership_rows(state, bucket).await {
            merged.entry(pc_id).or_default().extend(g.groups);
        }
    }
    merged
        .into_iter()
        .map(|(pc, set)| (pc, set.into_iter().collect()))
        .collect()
}

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
    /// Notification email addresses for this group (`group_contacts`
    /// KV). Empty when none are set. Drives the Groups page email column.
    pub emails: Vec<String>,
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
    // Pass 1 — membership: the effective (manual ∪ derived, #1032①) map,
    // inverted pc -> groups into group -> [pc_ids]. Best-effort: an
    // unavailable bucket degrades to "no members" (the config-only / contacts
    // passes below still surface those groups).
    let mut by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (pc_id, groups) in effective_membership(&state).await {
        for name in groups {
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
    // A group can exist solely via a contacts entry (email set before
    // any PC joined), so fold those names into the union too.
    let mut contacts = super::group_contacts::contacts_map(&state).await;
    all_names.extend(contacts.keys().cloned());

    let groups = all_names
        .into_iter()
        .map(|name| {
            let mut members = by_group.remove(&name).unwrap_or_default();
            members.sort();
            let emails = contacts.remove(&name).unwrap_or_default();
            GroupSummary {
                has_config: with_config.contains(&name),
                emails,
                name,
                members,
            }
        })
        .collect();

    Ok(Json(GroupsOverview { groups }))
}

/// Build the fleet's `pc_id -> [group]` membership map from the
/// `agent_groups` bucket — the same walk [`list_all_groups`] does, but
/// returned raw (not inverted, no config-only groups) for callers that
/// need to expand a group-targeted address to its member PCs (the
/// notifications audience resolver). PCs with no groups are omitted.
///
/// Best-effort: a bucket-open / key-read failure degrades to an empty
/// map rather than propagating — the caller then resolves only the
/// explicitly-addressed PCs, which is a safe partial answer for a
/// read-only confirmation view.
pub(crate) async fn membership_map(state: &AppState) -> HashMap<String, Vec<String>> {
    // Effective membership (manual ∪ derived, #1032①), dropping PCs with no
    // groups so a group-targeted notification's audience counts a PC that is a
    // member only via a dynamic group.
    effective_membership(state)
        .await
        .into_iter()
        .filter(|(_, groups)| !groups.is_empty())
        .collect()
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
