//! Admin API for the layered `agent_config` KV bucket (Sprint 6).
//!
//! Routes:
//!   GET    /api/config                        -> global ConfigScope
//!   PUT    /api/config                        (replace global scope)
//!   DELETE /api/config                        (drop the global row)
//!   PUT    /api/config/fields/{field}         body {"value": "…"}
//!   DELETE /api/config/fields/{field}
//!     -> set / clear ONE field of the global scope. Same pair under
//!        /api/groups/{name}/config/fields/{field} and
//!        /api/pcs/{pc_id}/config/fields/{field}. Done as a
//!        compare-and-swap read-modify-write on the server, so a
//!        concurrent writer of a *different* field on the same scope
//!        (e.g. a rollout writing `target_version`) is never clobbered;
//!        answers {scope, changed} and writes nothing when already
//!        satisfied.
//!   GET    /api/config/defaults               -> built-in EffectiveConfig
//!     (compiled-in floor values; read-only placeholder source for
//!      the SPA global editor)
//!   GET    /api/groups/{name}/config          -> group ConfigScope
//!   PUT    /api/groups/{name}/config          (replace group scope)
//!   DELETE /api/groups/{name}/config          (drop the row)
//!   GET    /api/groups/{name}/config/inherited
//!     -> EffectiveConfig a group scope layers on (built-in→global)
//!   GET    /api/pcs/{pc_id}/config            -> pc ConfigScope
//!   PUT    /api/pcs/{pc_id}/config            (replace pc scope)
//!   DELETE /api/pcs/{pc_id}/config            (drop the row)
//!   GET    /api/pcs/{pc_id}/config/inherited
//!     -> EffectiveConfig the PC inherits with its own scope excluded
//!        (built-in→global→groups). Read-only placeholder source for
//!        the SPA's per-scope editors.
//!   GET    /api/agents/{pc_id}/effective_config
//!     -> the resolved EffectiveConfig + any ResolutionWarnings the
//!        resolver emitted. Read-only convenience for debugging
//!        "why is this PC running version X?"
//!
//! All handlers go straight at the JetStream KV bucket — no SQLite
//! projection. The agent-side config_supervisor watches the same
//! bucket and reconciles within one NATS round-trip of the write.
//!
//! Every mutation records an audit event attributed to the caller. The
//! event is published after the KV write succeeds and is best-effort
//! (see `audit::record`), so it is not atomic with the write. The
//! whole-scope PUTs remain blind puts; only the field routes are
//! compare-and-swap.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use serde::Serialize;
use tracing::{info, warn};

use kanade_shared::config_field::{FieldUpdate, FieldValue, apply_field};

use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, KEY_AGENT_CONFIG_GLOBAL, agent_config_group_key,
    agent_config_pc_key, parse_agent_config_group_key,
};
use kanade_shared::wire::{AgentGroups, ConfigScope, EffectiveConfig, ResolutionWarning, resolve};

use super::AppState;
use crate::audit::{self, Caller};

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
    caller: Caller,
    Json(scope): Json<ConfigScope>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    write_scope(&kv, KEY_AGENT_CONFIG_GLOBAL, &scope).await?;
    info!(scope = ?scope, "agent_config.global replaced");
    audit_scope_put(&state, &caller, KEY_AGENT_CONFIG_GLOBAL, &scope).await;
    Ok(Json(scope))
}

pub async fn delete_global(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    delete_key(&kv, KEY_AGENT_CONFIG_GLOBAL).await?;
    info!("agent_config.global deleted");
    audit_scope_clear(&state, &caller, KEY_AGENT_CONFIG_GLOBAL).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_field_global(
    State(state): State<AppState>,
    caller: Caller,
    Path(field): Path<String>,
    Json(body): Json<FieldValue>,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    set_field(&state, &caller, KEY_AGENT_CONFIG_GLOBAL, &field, body.value).await
}

pub async fn unset_field_global(
    State(state): State<AppState>,
    caller: Caller,
    Path(field): Path<String>,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    unset_field(&state, &caller, KEY_AGENT_CONFIG_GLOBAL, &field).await
}

/// Built-in default [`EffectiveConfig`] — the floor every scope
/// layers on top of. Read-only and state-free (the values are
/// compiled in), so it needs no KV round-trip. The SPA's global
/// editor reads this to show each field's inherited default as a
/// placeholder: the operator sees what a left-blank field resolves
/// to without having to pin the value into the global scope (pinning
/// a default would freeze it against future #491-style default
/// changes). Sourcing it here keeps Rust the single source of truth
/// rather than duplicating the floor values in TypeScript.
pub async fn defaults() -> Json<EffectiveConfig> {
    Json(EffectiveConfig::builtin_defaults())
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
    caller: Caller,
    Path(name): Path<String>,
    Json(scope): Json<ConfigScope>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_group_key(&name);
    write_scope(&kv, &key, &scope).await?;
    info!(group = %name, scope = ?scope, "agent_config.groups.<name> replaced");
    audit_scope_put(&state, &caller, &key, &scope).await;
    Ok(Json(scope))
}

pub async fn delete_group(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_group_key(&name);
    delete_key(&kv, &key).await?;
    info!(group = %name, "agent_config.groups.<name> deleted");
    audit_scope_clear(&state, &caller, &key).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_field_group(
    State(state): State<AppState>,
    caller: Caller,
    Path((name, field)): Path<(String, String)>,
    Json(body): Json<FieldValue>,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    set_field(
        &state,
        &caller,
        &agent_config_group_key(&name),
        &field,
        body.value,
    )
    .await
}

pub async fn unset_field_group(
    State(state): State<AppState>,
    caller: Caller,
    Path((name, field)): Path<(String, String)>,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    unset_field(&state, &caller, &agent_config_group_key(&name), &field).await
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
    caller: Caller,
    Path(pc_id): Path<String>,
    Json(scope): Json<ConfigScope>,
) -> Result<Json<ConfigScope>, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_pc_key(&pc_id);
    write_scope(&kv, &key, &scope).await?;
    info!(pc_id = %pc_id, scope = ?scope, "agent_config.pcs.<pc_id> replaced");
    audit_scope_put(&state, &caller, &key, &scope).await;
    Ok(Json(scope))
}

pub async fn delete_pc(
    State(state): State<AppState>,
    caller: Caller,
    Path(pc_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = open_cfg(&state).await?;
    let key = agent_config_pc_key(&pc_id);
    delete_key(&kv, &key).await?;
    info!(pc_id = %pc_id, "agent_config.pcs.<pc_id> deleted");
    audit_scope_clear(&state, &caller, &key).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_field_pc(
    State(state): State<AppState>,
    caller: Caller,
    Path((pc_id, field)): Path<(String, String)>,
    Json(body): Json<FieldValue>,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    set_field(
        &state,
        &caller,
        &agent_config_pc_key(&pc_id),
        &field,
        body.value,
    )
    .await
}

pub async fn unset_field_pc(
    State(state): State<AppState>,
    caller: Caller,
    Path((pc_id, field)): Path<(String, String)>,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    unset_field(&state, &caller, &agent_config_pc_key(&pc_id), &field).await
}

// -------- single-field updates --------

async fn set_field(
    state: &AppState,
    caller: &Caller,
    key: &str,
    field: &str,
    value: String,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    let kv = open_cfg(state).await?;
    let update = update_field(&kv, key, field, Some(&value)).await?;
    info!(
        key,
        field,
        changed = update.changed,
        "agent_config field set"
    );
    audit::record(
        &state.nats,
        "operator",
        "config_field_set",
        Some(key),
        Some(caller),
        serde_json::json!({ "field": field, "value": value, "changed": update.changed }),
    )
    .await;
    Ok(Json(update))
}

async fn unset_field(
    state: &AppState,
    caller: &Caller,
    key: &str,
    field: &str,
) -> Result<Json<FieldUpdate>, (StatusCode, String)> {
    let kv = open_cfg(state).await?;
    let update = update_field(&kv, key, field, None).await?;
    info!(
        key,
        field,
        changed = update.changed,
        "agent_config field unset"
    );
    audit::record(
        &state.nats,
        "operator",
        "config_field_unset",
        Some(key),
        Some(caller),
        serde_json::json!({ "field": field, "changed": update.changed }),
    )
    .await;
    Ok(Json(update))
}

/// Set (`Some`) or clear (`None`) one field of the scope at `key` as a
/// compare-and-swap read-modify-write, so a concurrent writer of
/// another field on the same scope is never lost. An update that would
/// leave the scope unchanged skips the write entirely (no revision
/// bump, no watcher wake).
///
/// The field/value is validated first, with the same grammar and
/// message the CLI uses, and a rejection is a 400 — not something to
/// discover on every CAS retry.
async fn update_field(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
    field: &str,
    value: Option<&str>,
) -> Result<FieldUpdate, (StatusCode, String)> {
    validate_field(field, value)?;
    let mut changed = false;
    let scope = kanade_shared::kv_cas::read_modify_write(kv, key, |scope: &mut ConfigScope| {
        let before = scope.clone();
        // Pre-validated above, so Err is unreachable here.
        let _ = apply_field(scope, field, value);
        // Overwritten on every CAS round; the last round is the one
        // that decided whether a write happened.
        changed = *scope != before;
        changed
    })
    .await
    .map_err(|e| {
        warn!(error = %e, key, field, "agent_config field update");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update {key}: {e:#}"),
        )
    })?;
    Ok(FieldUpdate { scope, changed })
}

/// Reject a bad field/value as a 400 carrying the CLI's exact message
/// (`{e:#}` keeps the context chain the CLI prints).
fn validate_field(field: &str, value: Option<&str>) -> Result<(), (StatusCode, String)> {
    apply_field(&mut ConfigScope::default(), field, value)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))
}

async fn audit_scope_put(state: &AppState, caller: &Caller, key: &str, scope: &ConfigScope) {
    audit::record(
        &state.nats,
        "operator",
        "config_scope_put",
        Some(key),
        Some(caller),
        serde_json::json!({ "scope": scope }),
    )
    .await;
}

async fn audit_scope_clear(state: &AppState, caller: &Caller, key: &str) {
    audit::record(
        &state.nats,
        "operator",
        "config_scope_clear",
        Some(key),
        Some(caller),
        serde_json::json!({}),
    )
    .await;
}

// -------- resolved view --------

#[derive(Serialize)]
pub struct EffectiveConfigResponse {
    pub pc_id: String,
    pub effective: EffectiveConfig,
    pub warnings: Vec<String>,
    /// The groups the PC belongs to, as the resolver saw them — the
    /// answer to "which group layers applied?" without a second request.
    pub my_groups: Vec<String>,
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

    let (global_scope, group_scopes) = collect_global_and_groups(&cfg_kv).await?;
    let pc_scope = read_optional_scope(&cfg_kv, &agent_config_pc_key(&pc_id)).await?;
    let my_groups = pc_group_memberships(&groups_kv, &pc_id).await;

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
        my_groups,
    }))
}

/// The EffectiveConfig a PC would resolve to **if its own
/// `pcs.<pc_id>` scope were blank** — built-in → global → its groups,
/// with the per-PC layer excluded. The SPA's PC editor renders these
/// as per-field placeholders so an operator can see what each field
/// falls back to when left blank, the same way the global editor uses
/// `/api/config/defaults`. Read-only; warnings are irrelevant for a
/// placeholder view and dropped.
pub async fn pc_inherited(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
) -> Result<Json<EffectiveConfig>, (StatusCode, String)> {
    let cfg_kv = open_cfg(&state).await?;
    let groups_kv = open_groups(&state).await?;

    let (global_scope, group_scopes) = collect_global_and_groups(&cfg_kv).await?;
    let my_groups = pc_group_memberships(&groups_kv, &pc_id).await;

    // pc_scope = None → the PC's own overrides are excluded.
    let (inherited, _warns) = resolve(global_scope.as_ref(), &group_scopes, None, &my_groups);
    Ok(Json(inherited))
}

/// The base a group scope layers on top of: built-in → global only.
/// A group's *other* layers (sibling groups, the per-PC scope) are
/// resolved per-PC and can't be determined from the group name alone,
/// so this deliberately shows just the built-in→global base; the SPA
/// hints that sibling-group overrides aren't reflected here. The
/// `name` path segment is unused today but keeps the route symmetric
/// with the per-group config path (and lets us refine this later).
pub async fn group_inherited(
    State(state): State<AppState>,
    Path(_name): Path<String>,
) -> Result<Json<EffectiveConfig>, (StatusCode, String)> {
    let cfg_kv = open_cfg(&state).await?;
    let global_scope = read_optional_scope(&cfg_kv, KEY_AGENT_CONFIG_GLOBAL).await?;
    let (inherited, _warns) = resolve(global_scope.as_ref(), &BTreeMap::new(), None, &[]);
    Ok(Json(inherited))
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

/// Read the global scope and every `groups.<name>` scope from the
/// agent_config bucket in one pass — the shared half of resolving any
/// PC's or group's effective config (`effective` and `pc_inherited`).
async fn collect_global_and_groups(
    cfg_kv: &async_nats::jetstream::kv::Store,
) -> Result<(Option<ConfigScope>, BTreeMap<String, ConfigScope>), (StatusCode, String)> {
    let global_scope = read_optional_scope(cfg_kv, KEY_AGENT_CONFIG_GLOBAL).await?;

    // Walk every key in agent_config so we build the same group view
    // the agent would, minus the watch loop.
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

    Ok((global_scope, group_scopes))
}

/// A PC's group memberships from the agent_groups bucket. Returns an
/// empty list when the PC has no row yet (a fresh, unassigned agent —
/// the normal case) AND, deliberately, on a transient KV read or decode
/// error: resolving against "no groups" keeps `effective`/`pc_inherited`
/// answering (degraded) rather than 500-ing the whole config view when
/// the agent_groups bucket hiccups. A read/decode failure is logged
/// (not silent) so it's diagnosable — only `Ok(None)` is truly quiet.
async fn pc_group_memberships(
    groups_kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
) -> Vec<String> {
    match groups_kv.get(pc_id).await {
        Ok(Some(bytes)) => match serde_json::from_slice::<AgentGroups>(&bytes) {
            Ok(g) => g.groups,
            Err(e) => {
                warn!(error = %e, pc_id, "decode AgentGroups — treating as no groups");
                Vec::new()
            }
        },
        Ok(None) => Vec::new(),
        Err(e) => {
            warn!(error = %e, pc_id, "read agent_groups — treating as no groups");
            Vec::new()
        }
    }
}

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

#[cfg(test)]
mod tests {
    //! `update_field` against a real JetStream KV. The CAS and the
    //! "unchanged ⇒ no revision bump" behaviour are properties of the
    //! broker, so a mock would only restate the implementation. Like the
    //! `kv_cas` live suite these are `#[ignore]`d (each spawns a throwaway
    //! `nats-server -js`, which must be in PATH) and run with:
    //!
    //! ```text
    //! cargo test -p kanade-backend agent_config -- --ignored
    //! ```

    use std::process::Stdio;
    use std::time::Duration;

    use super::*;

    struct Harness {
        kv: async_nats::jetstream::kv::Store,
        _server: tokio::process::Child,
        _storage: tempfile::TempDir,
    }

    async fn harness() -> Harness {
        let port = portpicker::pick_unused_port().expect("pick port");
        let storage = tempfile::TempDir::new().expect("storage tempdir");
        let server = tokio::process::Command::new("nats-server")
            .arg("-js")
            .arg("-p")
            .arg(port.to_string())
            .arg("-sd")
            .arg(storage.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server (is it in PATH?)");
        let url = format!("nats://127.0.0.1:{port}");
        let mut client = None;
        for _ in 0..50 {
            match async_nats::connect(&url).await {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let js = async_nats::jetstream::new(client.expect("nats-server did not come up in 5s"));
        let kv = js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: BUCKET_AGENT_CONFIG.to_string(),
                history: 5,
                ..Default::default()
            })
            .await
            .expect("create agent_config bucket");
        Harness {
            kv,
            _server: server,
            _storage: storage,
        }
    }

    async fn revision(kv: &async_nats::jetstream::kv::Store, key: &str) -> Option<u64> {
        kv.entry(key).await.unwrap().map(|e| e.revision)
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn concurrent_sets_of_different_fields_both_survive() {
        let h = harness().await;
        let key = agent_config_group_key("canary");
        // A rollout-style writer and an operator-style writer on the same
        // scope, plus extra writers to force CAS conflicts.
        let mut tasks = Vec::new();
        for (field, value) in [
            ("target_version", "1.2.3"),
            ("heartbeat_interval", "15s"),
            ("host_perf_interval", "2m"),
            ("target_version_jitter", "30m"),
            ("process_perf_top_n", "20"),
            ("client_display_name", "tool"),
        ] {
            let kv = h.kv.clone();
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                update_field(&kv, &key, field, Some(value)).await.unwrap()
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().changed);
        }
        let scope = read_scope_or_default(&h.kv, &key).await.unwrap();
        assert_eq!(scope.target_version.as_deref(), Some("1.2.3"));
        assert_eq!(scope.heartbeat_interval.as_deref(), Some("15s"));
        assert_eq!(scope.host_perf_interval.as_deref(), Some("2m"));
        assert_eq!(scope.target_version_jitter.as_deref(), Some("30m"));
        assert_eq!(scope.process_perf_top_n, Some(20));
        assert_eq!(scope.client_display_name.as_deref(), Some("tool"));
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn unchanged_set_and_unset_do_not_bump_the_revision() {
        let h = harness().await;
        let key = agent_config_pc_key("PC-01");

        // Unset on a row that was never created: no write, no row.
        let r = update_field(&h.kv, &key, "target_version", None)
            .await
            .unwrap();
        assert!(!r.changed);
        assert_eq!(revision(&h.kv, &key).await, None);

        let r = update_field(&h.kv, &key, "heartbeat_interval", Some("15s"))
            .await
            .unwrap();
        assert!(r.changed);
        let rev = revision(&h.kv, &key).await.unwrap();

        // Same value again, and an unset of an unset field.
        let r = update_field(&h.kv, &key, "heartbeat_interval", Some("15s"))
            .await
            .unwrap();
        assert!(!r.changed);
        assert_eq!(r.scope.heartbeat_interval.as_deref(), Some("15s"));
        let r = update_field(&h.kv, &key, "target_version", None)
            .await
            .unwrap();
        assert!(!r.changed);
        assert_eq!(revision(&h.kv, &key).await, Some(rev));

        // A real unset does write.
        let r = update_field(&h.kv, &key, "heartbeat_interval", None)
            .await
            .unwrap();
        assert!(r.changed);
        assert!(r.scope.heartbeat_interval.is_none());
        assert!(revision(&h.kv, &key).await.unwrap() > rev);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn field_update_recreates_a_deleted_row_and_global_row_can_be_cleared() {
        let h = harness().await;
        update_field(
            &h.kv,
            KEY_AGENT_CONFIG_GLOBAL,
            "target_version",
            Some("1.0.0"),
        )
        .await
        .unwrap();
        delete_key(&h.kv, KEY_AGENT_CONFIG_GLOBAL).await.unwrap();
        assert!(
            read_optional_scope(&h.kv, KEY_AGENT_CONFIG_GLOBAL)
                .await
                .unwrap()
                .is_none()
        );
        // Unset on the deleted row is a no-op; a set re-creates it.
        let r = update_field(&h.kv, KEY_AGENT_CONFIG_GLOBAL, "target_version", None)
            .await
            .unwrap();
        assert!(!r.changed);
        let r = update_field(
            &h.kv,
            KEY_AGENT_CONFIG_GLOBAL,
            "target_version",
            Some("2.0.0"),
        )
        .await
        .unwrap();
        assert!(r.changed);
        assert_eq!(r.scope.target_version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn invalid_field_is_a_400_with_the_cli_message() {
        for (field, value, needle) in [
            ("nope", Some("x"), "unknown field 'nope'"),
            ("heartbeat_interval", Some("soon"), "humantime duration"),
            ("process_perf_enabled", Some("maybe"), "expected true|false"),
            (
                "max_local_concurrent",
                Some("0"),
                "expected an integer >= 1",
            ),
        ] {
            let (status, msg) = validate_field(field, value).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(msg.contains(needle), "{field}: {msg}");
        }
        assert!(validate_field("heartbeat_interval", Some("15s")).is_ok());
        assert!(validate_field("heartbeat_interval", None).is_ok());
    }
}
