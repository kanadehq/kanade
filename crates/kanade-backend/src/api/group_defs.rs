//! Group-definition CRUD (#1032) — operator-registered [`GroupDef`] resources
//! in `BUCKET_GROUP_DEFS`. A group definition names a fleet group either
//! statically (a literal `members:` list) or dynamically (a read-only `query:`
//! returning a `pc_id` column). The scheduler's `resolve_roster` resolves a
//! schedule's `target.groups` against these in addition to the imperative
//! `agent_groups` membership — the two coexist and this never mutates
//! `agent_groups`.
//!
//! Mirrors the `views` resource spine: JSON catalog in `BUCKET_GROUP_DEFS`
//! (what the backend + scheduler read), with a best-effort comment-preserving
//! YAML mirror in `BUCKET_GROUP_DEFS_YAML` for the SPA editor.

use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use futures::TryStreamExt;
use kanade_shared::kv::{BUCKET_GROUP_DEFS, BUCKET_GROUP_DEFS_YAML};
use kanade_shared::manifest::GroupDef;
use serde::Serialize;
use tracing::{info, warn};

use crate::api::AppState;
use crate::api::yaml_body::{YamlOrJson, mirror_yaml, yaml_headers};
use crate::audit;
use crate::audit::Caller;

/// Response for `POST /api/group-defs` — the upserted group's id plus a compact
/// shape summary. (`GET /api/group-defs` returns full [`GroupDef`] objects.)
#[derive(Serialize)]
pub struct GroupDefSummary {
    pub id: String,
    pub description: Option<String>,
    /// `"static"` (has `members:`) or `"dynamic"` (has `query:`).
    pub kind: &'static str,
    /// For a static group, the number of declared members; `None` for a
    /// dynamic group (its size is only known once the query runs).
    pub member_count: Option<usize>,
    pub tags: Vec<String>,
}

/// Members-preview response for `GET /api/group-defs/{id}/members` — the
/// resolved `pc_id` set. Lets an operator (or the SPA) see exactly who a group
/// covers before wiring it into a schedule's `target`.
#[derive(Serialize)]
pub struct GroupMembers {
    pub id: String,
    pub kind: &'static str,
    pub count: usize,
    pub members: Vec<String>,
}

fn kind_of(g: &GroupDef) -> &'static str {
    if g.dynamic_query().is_some() {
        "dynamic"
    } else {
        "static"
    }
}

/// Reject a malformed `{id}` path parameter at the API boundary before it
/// becomes a NATS KV key. The CLI validates too, but a direct API client could
/// send path-traversal / out-of-charset ids; validate here so every id-taking
/// handler (get_yaml / members / delete) is safe on its own. Same charset as
/// `GroupDef::validate` and the `views` resource.
fn guard_id(id: &str) -> Result<(), (StatusCode, String)> {
    if kanade_shared::manifest::is_valid_resource_id(id) {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("invalid group id '{id}' (allowed: [A-Za-z0-9._-])"),
        ))
    }
}

/// `GET /api/group-defs` — list every registered group definition
/// (viewer-readable).
pub async fn list(State(s): State<AppState>) -> Result<Json<Vec<GroupDef>>, (StatusCode, String)> {
    let kv = match s.jetstream.get_key_value(BUCKET_GROUP_DEFS).await {
        Ok(k) => k,
        // Bucket not created yet (no group ever registered) ⇒ empty list, not
        // an error — matches views::list.
        Err(_) => return Ok(Json(Vec::new())),
    };
    let keys: Vec<String> = kv
        .keys()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?
        .try_collect()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?;
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        match kv.get(&k).await {
            Ok(Some(bytes)) => match serde_json::from_slice::<GroupDef>(&bytes) {
                Ok(g) => out.push(g),
                // Surface a corrupt entry instead of silently dropping it, but
                // skip so one bad row doesn't fail the whole list.
                Err(e) => {
                    warn!(group_id = %k, error = %e, "group_defs: skipping undecodable entry")
                }
            },
            Ok(None) => {}
            Err(e) => warn!(group_id = %k, error = %e, "group_defs: kv get failed, skipping"),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(out))
}

/// `POST /api/group-defs` — create/upsert a group definition (operator).
/// Validates, re-checks a dynamic `query:` read-only at this write boundary,
/// writes the JSON catalog entry, and best-effort mirrors the operator YAML.
pub async fn create(
    State(s): State<AppState>,
    caller: Caller,
    body: YamlOrJson<GroupDef>,
) -> Result<Json<GroupDefSummary>, (StatusCode, String)> {
    let YamlOrJson {
        value: group,
        raw_yaml,
    } = body;

    if let Err(e) = group.validate() {
        return Err((StatusCode::BAD_REQUEST, format!("invalid group: {e}")));
    }
    // A dynamic group's `query` runs in the read-only sandbox (`api::query`).
    // The RO connection enforces it at run time too, but rejecting a write /
    // DDL / stacked statement HERE means the operator learns at `group create`
    // instead of watching membership silently fail on every resolve.
    // `GroupDef::validate` (kanade-shared) can't do this — it can't depend on
    // the backend sandbox — so it lives at this write boundary (same split as
    // `views::create`).
    if let Some(query) = group.dynamic_query()
        && let Err(e) = crate::api::query::validate_read_only(query)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("invalid group: query is not a read-only query: {e}"),
        ));
    }

    let kv = s
        .jetstream
        .create_key_value(KvConfig {
            bucket: BUCKET_GROUP_DEFS.into(),
            history: 5,
            ..Default::default()
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ensure KV: {e}")))?;

    let body_bytes = serde_json::to_vec(&group)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
    kv.put(&group.id, body_bytes.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;

    // Invalidate the resolver cache for this id so an edited query/members
    // takes effect immediately rather than after the previous entry's TTL.
    // (The content-hash miss already covers this, but dropping the entry is
    // cheaper than a stale-then-recompute and makes an edit deterministic.)
    // `.expect` on poison, consistent with `group_sql`'s cache access — a
    // poisoned cache mutex is an unreachable bug (the lock is only ever held
    // for trivial map ops), not a condition to silently swallow here while
    // panicking there.
    s.group_cache
        .lock()
        .expect("group_cache mutex")
        .remove(&group.id);

    // Operator-facing YAML mirror — best-effort (same as views); the backend
    // reads the JSON catalog, the YAML store only feeds the SPA editor.
    let yaml_source = raw_yaml.unwrap_or_else(|| {
        serde_yaml::to_string(&group)
            .unwrap_or_else(|_| String::from("# YAML mirror unavailable for this entry"))
    });
    if let Err(e) = mirror_yaml(&s, BUCKET_GROUP_DEFS_YAML, &group.id, &yaml_source).await {
        warn!(error = %e, group_id = %group.id, "group_defs: YAML mirror put failed; JSON catalog is current");
    }

    let kind = kind_of(&group);
    let member_count = (kind == "static").then_some(group.members.len());
    info!(group_id = %group.id, kind, "group def upserted");
    audit::record(
        &s.nats,
        "operator",
        "group_def_upsert",
        Some(&group.id),
        Some(&caller),
        serde_json::json!({ "kind": kind }),
    )
    .await;

    Ok(Json(GroupDefSummary {
        id: group.id,
        description: group.description,
        kind,
        member_count,
        tags: group.tags,
    }))
}

/// `GET /api/group-defs/{id}/yaml` — operator source YAML (mirror first, else a
/// serde_yaml dump of the JSON catalog entry).
pub async fn get_yaml(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, HeaderMap, String), (StatusCode, String)> {
    guard_id(&id)?;
    if let Ok(kv) = s.jetstream.get_key_value(BUCKET_GROUP_DEFS_YAML).await
        && let Ok(Some(bytes)) = kv.get(&id).await
        && let Ok(text) = String::from_utf8(bytes.to_vec())
    {
        return Ok((StatusCode::OK, yaml_headers(), text));
    }
    let kv = s
        .jetstream
        .get_key_value(BUCKET_GROUP_DEFS)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, format!("group '{id}' not found")))?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("group '{id}' not found")))?;
    let group: GroupDef = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("decode: {e}")))?;
    let yaml = serde_yaml::to_string(&group).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode YAML: {e}"),
        )
    })?;
    Ok((StatusCode::OK, yaml_headers(), yaml))
}

/// `GET /api/group-defs/{id}/members` — resolve the group and return its
/// `pc_id` set (viewer-readable). For a static group this is its literal
/// members; for a dynamic group it runs the query (cached on `refresh`). A
/// query error surfaces as a 400 so an operator can debug the SQL.
pub async fn members(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<GroupMembers>, (StatusCode, String)> {
    guard_id(&id)?;
    let kv = s
        .jetstream
        .get_key_value(BUCKET_GROUP_DEFS)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, format!("group '{id}' not found")))?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("group '{id}' not found")))?;
    let group: GroupDef = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("decode: {e}")))?;

    let kind = kind_of(&group);
    let mut members = crate::api::group_sql::resolve_group_members(&s, &group)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("resolve group '{id}': {e}"),
            )
        })?;
    members.sort();
    members.dedup();
    Ok(Json(GroupMembers {
        id,
        kind,
        count: members.len(),
        members,
    }))
}

/// `DELETE /api/group-defs/{id}` — remove a group definition (operator).
/// Best-effort drops the YAML mirror + resolver cache entry too.
pub async fn delete(
    State(s): State<AppState>,
    Path(id): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    guard_id(&id)?;
    let kv = match s.jetstream.get_key_value(BUCKET_GROUP_DEFS).await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "group_defs KV missing on delete");
            return Err((StatusCode::NOT_FOUND, "group_defs bucket missing".into()));
        }
    };
    // NATS KV `delete` tombstones even a never-written key, so guard on
    // existence first — otherwise deleting a typo'd id would 204 and write a
    // phantom `group_def_delete` audit record with no matching upsert.
    if kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv get: {e}")))?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, format!("group '{id}' not found")));
    }
    kv.delete(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv delete: {e}")))?;
    if let Ok(ykv) = s.jetstream.get_key_value(BUCKET_GROUP_DEFS_YAML).await {
        let _ = ykv.delete(&id).await;
    }
    s.group_cache.lock().expect("group_cache mutex").remove(&id);
    info!(group_id = %id, "group def deleted");
    audit::record(
        &s.nats,
        "operator",
        "group_def_delete",
        Some(&id),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
