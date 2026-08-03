//! HTTP surface for the staged self-update flow.
//!
//! * `POST /api/agents/publish` — multipart upload (`file` =
//!   binary, `version` = label, required for non-PE binaries) →
//!   puts the bytes in the `agent_releases` Object Store under a
//!   platform-derived key (bare `<version>` for Windows,
//!   `<version>-linux-<arch>` for Linux — see
//!   kanade_shared::bin_platform). Mirrors `kanade agent
//!   publish` on the CLI side; the SPA's Rollout page wires a
//!   file picker to this endpoint.
//! * `GET  /api/agents/releases` — list every version present in
//!   the Object Store. Used by the Web UI's rollout picker.
//! * `POST /api/agents/rollout` — flip `target_version` (and
//!   optionally `target_version_jitter`) on one scope of the
//!   layered `agent_config` bucket. Mirrors the CLI's `rollout`
//!   subcommand.

use axum::Json;
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use kanade_shared::bin_platform::{
    AgentPlatform, base_version_of_key, candidate_keys, platform_of_key,
};
use kanade_shared::exe_version::extract_pe_version;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, KEY_AGENT_CONFIG_GLOBAL, OBJECT_AGENT_RELEASES, agent_config_group_key,
    agent_config_pc_key, parse_agent_config_group_key, parse_agent_config_pc_key,
};
use kanade_shared::wire::ConfigScope;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::AppState;
use crate::audit;
use crate::audit::Caller;

// ─── POST /api/agents/publish ────────────────────────────────────────

#[derive(Serialize)]
pub struct PublishResponse {
    pub version: String,
    /// The Object Store key the binary was stored under — `version` for
    /// Windows, `<version>-linux-<arch>` for Linux (bin_platform key scheme).
    pub key: String,
    pub platform: String,
    pub size: u64,
    pub digest: Option<String>,
}

pub async fn publish(
    State(state): State<AppState>,
    caller: Caller,
    mut multipart: Multipart,
) -> Result<Json<PublishResponse>, (StatusCode, String)> {
    // The Object Store key depends on the binary's platform
    // (kanade_shared::bin_platform): a Windows PE is keyed by its embedded
    // VERSIONINFO (making a "label vs binary version" disagreement
    // physically impossible — the failure mode that caused the v0.13.0
    // "1.0.0"-loop incident), a Linux ELF has no such resource and takes
    // its label from the `version` form field.
    let mut bytes: Option<Vec<u8>> = None;
    let mut version_field: Option<String> = None;

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
            "version" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("read version field: {e}")))?;
                let text = text.trim().to_string();
                if !text.is_empty() {
                    version_field = Some(text);
                }
            }
            other => {
                warn!(field = other, "publish: ignoring unknown multipart field");
            }
        }
    }

    let bytes = bytes.ok_or((StatusCode::BAD_REQUEST, "missing 'file' field".into()))?;
    if bytes.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "'file' field is empty".into()));
    }

    let platform = AgentPlatform::detect(&bytes).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let version = match platform {
        AgentPlatform::WindowsX86_64 | AgentPlatform::WindowsAarch64 => {
            let pe = extract_pe_version(&bytes).ok_or((
                StatusCode::BAD_REQUEST,
                "couldn't extract VERSIONINFO from the uploaded binary — \
                 is it a Windows PE built with `winres`? Kanade ≥ v0.13.1 \
                 embeds the resource automatically; older binaries need to \
                 be re-published from a current build."
                    .to_owned(),
            ))?;
            // The CLI's "must agree" rule (#270-era, resolve_publish_version):
            // a form field that contradicts the embedded version is rejected
            // rather than silently ignored. Comparison ignores a leading `v`
            // and surrounding whitespace, like the CLI.
            if let Some(v) = version_field.as_deref()
                && v.trim().trim_start_matches('v') != pe.trim().trim_start_matches('v')
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "version field '{v}' disagrees with the binary's embedded version '{pe}'; \
                         omit the field to use the embedded one, or pass the matching label"
                    ),
                ));
            }
            pe
        }
        AgentPlatform::LinuxX86_64 | AgentPlatform::LinuxAarch64 => version_field.ok_or((
            StatusCode::BAD_REQUEST,
            format!(
                "no version: the uploaded binary is a Linux ELF ({}), which carries no embedded \
                 VERSIONINFO — include a 'version' form field (e.g. X.Y.Z). A Windows PE built \
                 with `winres` (kanade ≥ v0.13.1) is auto-labelled.",
                platform.as_str()
            ),
        ))?,
    };

    let key = platform.release_key(&version);
    kanade_shared::bin_platform::check_release_key(&key)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let size = bytes.len() as u64;
    info!(
        version,
        key,
        platform = platform.as_str(),
        size,
        "publish: uploading new agent binary"
    );

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
    let meta = store.put(key.as_str(), &mut cursor).await.map_err(|e| {
        warn!(error = %e, "object_store.put");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    info!(version, key, digest = ?meta.digest, "publish: agent binary uploaded");

    // #1216: write-through so the SPA's immediate post-upload refetch
    // sees the release without racing the metadata watcher. Best-effort
    // — the watcher heals the index on the meta message.
    if let Err(e) =
        crate::projector::object_meta::apply(&state.pool, OBJECT_AGENT_RELEASES, &meta).await
    {
        warn!(error = %e, %version, "object_meta write-through failed (watcher will heal)");
    }

    audit::record(
        &state.nats,
        "operator",
        "agent_publish",
        Some(&key),
        Some(&caller),
        serde_json::json!({
            "version": version,
            "platform": platform.as_str(),
            "size": size,
            "digest": meta.digest,
        }),
    )
    .await;

    Ok(Json(PublishResponse {
        version,
        key,
        platform: platform.as_str().to_string(),
        size,
        digest: meta.digest,
    }))
}

/// `DELETE /api/agents/releases/<version>` — remove a release from
/// the Object Store. Rejects with 409 when any `agent_config` scope
/// still points at this version (global / per-group / per-pc), so an
/// operator can't accidentally remove a binary the fleet is rolling
/// out to. Pass the same path-parameter the listing endpoint
/// returns; the version is the Object Store key.
pub async fn delete_release(
    State(state): State<AppState>,
    Path(version): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = state
        .jetstream
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let mut keys = kv
        .keys()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    while let Some(k) = keys.next().await {
        let k = match k {
            Ok(k) => k,
            Err(_) => continue,
        };
        let entry = match kv.get(&k).await.map_err(|e| {
            warn!(error = %e, %k, "kv.get scope");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })? {
            Some(b) => b,
            None => continue,
        };
        let scope: ConfigScope = match serde_json::from_slice(&entry) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if scope.target_version.as_deref() == Some(base_version_of_key(&version)) {
            let base = base_version_of_key(&version);
            let label = if k == KEY_AGENT_CONFIG_GLOBAL {
                "global".to_string()
            } else if let Some(g) = parse_agent_config_group_key(&k) {
                format!("group:{g}")
            } else if let Some(p) = parse_agent_config_pc_key(&k) {
                format!("pc:{p}")
            } else {
                k.clone()
            };
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "version '{base}' is the current target_version of scope '{label}' — \
                     clear or change that scope first (kanade config unset target_version --… )"
                ),
            ));
        }
    }

    let store = state
        .jetstream
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    store.delete(&version).await.map_err(|e| {
        warn!(error = %e, %version, "object_store.delete");
        // async-nats returns a generic error if the key is missing;
        // surface that as 404 for a cleaner SPA experience.
        let msg = e.to_string();
        if msg.contains("not found") || msg.contains("no objects") {
            (
                StatusCode::NOT_FOUND,
                format!("version '{version}' not in Object Store"),
            )
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    })?;
    info!(%version, "publish: agent binary deleted");

    // #1216: write-through so the SPA's immediate post-delete refetch
    // no longer lists the release (watcher would heal, but slower).
    if let Err(e) =
        crate::projector::object_meta::delete_key(&state.pool, OBJECT_AGENT_RELEASES, &version)
            .await
    {
        warn!(error = %e, %version, "object_meta write-through failed (watcher will heal)");
    }

    audit::record(
        &state.nats,
        "operator",
        "agent_release_delete",
        Some(&version),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ─── GET /api/agents/releases ────────────────────────────────────────

#[derive(Serialize)]
pub struct ReleaseRow {
    pub version: String,
    /// Which platform this key's binary targets, parsed from the key's
    /// suffix: `"linux-x86_64"` / `"linux-aarch64"` for suffixed keys,
    /// `"windows"` for bare ones (the key scheme's definition — see
    /// kanade_shared::bin_platform).
    pub platform: String,
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
    // #1216: read the SQLite metadata index (projector::object_meta)
    // instead of ObjectStore::list() — the latter full-scanned the
    // stream per cold request (10 s on the measured bucket).
    let metas = crate::projector::object_meta::list_bucket(&state.pool, OBJECT_AGENT_RELEASES)
        .await
        .map_err(|e| {
            warn!(error = %e, "object_store_meta list agent_releases");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    let mut rows: Vec<ReleaseRow> = metas
        .into_iter()
        .map(|m| ReleaseRow {
            platform: platform_of_key(&m.key).to_string(),
            version: m.key,
            size: m.size as u64,
            digest: m.digest,
            modified: m.modified,
        })
        .collect();
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
    caller: Caller,
    Json(body): Json<RolloutBody>,
) -> Result<Json<RolloutResponse>, (StatusCode, String)> {
    let (key, label) = match &body.scope {
        RolloutScope::Global => (KEY_AGENT_CONFIG_GLOBAL.to_string(), "global".to_string()),
        RolloutScope::Group(g) => (agent_config_group_key(g), format!("group:{g}")),
        RolloutScope::Pc(p) => (agent_config_pc_key(p), format!("pc:{p}")),
    };

    // Normalize to the BASE version: a scope's target_version never
    // carries a platform suffix — the agent's own platform decides which
    // suffixed binary it fetches. The SPA picker already strips it; this
    // is defense in depth for every other caller.
    let version = base_version_of_key(&body.version).to_string();

    // Fail-fast on a version that doesn't have a binary uploaded yet.
    // A version passes when ANY of its keys exists: the bare Windows key
    // or a `<version>-linux-<arch>` one — a Linux-only publish must be
    // rollable-out too (Linux agents self-update from the suffixed key).
    let store = state
        .jetstream
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let mut any_key_exists = false;
    for candidate in candidate_keys(&version) {
        if store.info(&candidate).await.is_ok() {
            any_key_exists = true;
            break;
        }
    }
    if !any_key_exists {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "version '{version}' not found in {OBJECT_AGENT_RELEASES} — run `kanade agent publish` first"
            ),
        ));
    }

    let kv = state
        .jetstream
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    if let Some(j) = body.jitter.as_deref() {
        // #491: reject malformed jitter at the write boundary — the
        // agent's parse failure silently falls back, so a typo here
        // would otherwise disable the rollout stagger fleet-wide.
        humantime::parse_duration(j).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("jitter: expected a humantime duration (e.g. 30s, 10m, 1h): {e}"),
            )
        })?;
    }
    // #505: CAS read-modify-write — the previous get→put raced
    // concurrent writers on the same scope (e.g. a `config set`
    // updating heartbeat_interval) and clobbered their change.
    kanade_shared::kv_cas::read_modify_write(&kv, &key, |scope: &mut ConfigScope| {
        let before = scope.clone();
        scope.target_version = Some(version.clone());
        if let Some(j) = body.jitter.as_deref() {
            scope.target_version_jitter = Some(j.to_owned());
        }
        // Re-rolling-out the already-current version is a no-op:
        // skip the write so the revision doesn't bump and agents'
        // config watchers don't wake for nothing.
        *scope != before
    })
    .await
    .map_err(|e| {
        warn!(error = %e, %key, "rollout scope RMW");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update {BUCKET_AGENT_CONFIG}.{key}: {e:#}"),
        )
    })?;

    info!(
        scope = %label,
        version = %version,
        jitter = ?body.jitter,
        "rollout: target_version flipped via HTTP",
    );

    audit::record(
        &state.nats,
        "operator",
        "agent_rollout",
        Some(&key),
        Some(&caller),
        serde_json::json!({
            "version": version,
            "scope_label": label,
            "jitter": body.jitter,
        }),
    )
    .await;

    Ok(Json(RolloutResponse {
        version,
        scope_key: key,
        scope_label: label,
        jitter: body.jitter,
    }))
}
