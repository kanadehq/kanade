//! HTTP surface for the staged self-update flow.
//!
//! * `POST /api/agents/publish` — multipart upload (`file` =
//!   binary, `version` = label, required for non-PE binaries) →
//!   puts the bytes in the `agent_releases` Object Store under a
//!   platform-derived key (bare `<version>` for Windows,
//!   `<version>-linux-<arch>` for Linux, `<version>-macos-aarch64`
//!   for macOS, Apple Silicon only — see kanade_shared::bin_platform). Mirrors `kanade agent
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
use kanade_shared::bin_platform::{base_version_of_key, candidate_keys, platform_of_key};
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
    /// Windows, `<version>-linux-<arch>` for Linux, `<version>-macos-aarch64`
    /// for macOS (bin_platform key scheme; Apple Silicon only).
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
    // Admission (`kanade_shared::agent_metadata`): the upload must be an
    // official kanade-agent whose embedded product / platform / version
    // agree with the container and the optional `version` field. The key
    // comes from the embedded version, never the filename, and nothing
    // touches the Object Store until every check has passed.
    let mut bytes: Option<Vec<u8>> = None;
    let mut version_field: Option<String> = None;

    // A multipart read error carries its own status: the body-limit layer
    // surfaces as 413, which the CLI explains; a blanket 400 would hide that
    // an oversized upload is the problem.
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (e.status(), format!("read multipart field: {e}")))?
    {
        match field.name().unwrap_or("") {
            "file" => {
                if bytes.is_some() {
                    return Err((StatusCode::BAD_REQUEST, "duplicate 'file' field".into()));
                }
                let buf = field
                    .bytes()
                    .await
                    .map_err(|e| (e.status(), format!("read file field: {e}")))?;
                bytes = Some(buf.to_vec());
            }
            "version" => {
                if version_field.is_some() {
                    return Err((StatusCode::BAD_REQUEST, "duplicate 'version' field".into()));
                }
                let text = field
                    .text()
                    .await
                    .map_err(|e| (e.status(), format!("read version field: {e}")))?;
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

    let admitted =
        kanade_shared::agent_metadata::admit_agent_binary(&bytes, version_field.as_deref())
            .map_err(|e| {
                warn!(error = %e, "publish: agent binary refused at admission");
                (
                    StatusCode::BAD_REQUEST,
                    format!("agent publish refused: {e}"),
                )
            })?;
    let version = admitted.version;
    let platform = admitted.platform;
    let key = admitted.key;

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
                    "Object Store '{OBJECT_AGENT_RELEASES}' not found — the backend creates it at startup, so it was removed since or this broker is not the one expected; check `GET /api/jetstream/status` (restarting the backend recreates it)"
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

/// A rollout pinned to a pc_id the role-level NATS permissions cannot cover
/// would update an agent that then cannot start, so refuse it first.
fn check_rollout_scope(scope: &RolloutScope) -> Result<(), (StatusCode, String)> {
    if let RolloutScope::Pc(p) = scope {
        kanade_shared::subject::validate_pc_id(p).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    Ok(())
}

pub async fn rollout(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<RolloutBody>,
) -> Result<Json<RolloutResponse>, (StatusCode, String)> {
    check_rollout_scope(&body.scope)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rollout_pinned_to_a_pc_id_with_too_many_labels_is_refused() {
        assert!(check_rollout_scope(&RolloutScope::Pc("a.b.c.d".into())).is_ok());
        assert!(check_rollout_scope(&RolloutScope::Global).is_ok());
        let (st, msg) = check_rollout_scope(&RolloutScope::Pc("a.b.c.d.e".into())).unwrap_err();
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(msg.contains("at most 4"), "{msg}");
    }

    // ---- through the real router ----
    //
    // `POST /api/agents/publish` is the shared choke point for the CLI, the
    // fleet-deploy scripts and direct HTTP callers, so these drive the real
    // router (auth layered on as in `main.rs`) against a throwaway
    // `nats-server -js` and assert on the Object Stores themselves.
    //
    //   cargo test -p kanade-backend agent_releases -- --ignored

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use kanade_shared::agent_metadata::fixtures::{elf_x86_64, macho_arm64, meta, pe_x86_64};
    use kanade_shared::kv::OBJECT_APP_PACKAGES;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tower::ServiceExt;

    struct App {
        router: Router,
        js: async_nats::jetstream::Context,
        _server: tokio::process::Child,
        _storage: tempfile::TempDir,
    }

    fn mint(sub: &str) -> String {
        use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
        let claims = crate::auth::Claims {
            sub: sub.into(),
            exp: 4_102_444_800,
            aud: Some(crate::auth::EXPECTED_AUDIENCE.to_string()),
            roles: Vec::new(),
            allowed_features: None,
        };
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(crate::auth::signing_secret().as_bytes()),
        )
        .expect("mint")
    }

    async fn app() -> App {
        let port = portpicker::pick_unused_port().expect("pick port");
        let storage = tempfile::TempDir::new().expect("storage tempdir");
        let server = tokio::process::Command::new("nats-server")
            .args(["-js", "-p", &port.to_string(), "-sd"])
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
        let nats = client.expect("nats-server did not come up in 5s");
        let js = async_nats::jetstream::new(nats.clone());
        for bucket in [OBJECT_AGENT_RELEASES, OBJECT_APP_PACKAGES] {
            js.create_object_store(async_nats::jetstream::object_store::Config {
                bucket: bucket.to_string(),
                ..Default::default()
            })
            .await
            .expect("create object store");
        }

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, allowed_features) VALUES ('op', 'x', 'operator', NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let state = AppState {
            passkeys: None,
            pool: pool.clone(),
            query_pool: pool.clone(),
            commands: std::sync::Arc::new(crate::command_publisher::CommandPublisher::new(
                nats.clone(),
                None,
            )),
            nats,
            jetstream: js.clone(),
            explode_spec_cache: Default::default(),
            sql_view_cache: crate::api::view_sql::new_cache(),
            group_cache: crate::api::group_sql::new_cache(),
            mailer: None,
            public_url: None,
            nats_url: String::new(),
            login_throttle: Default::default(),
        };
        let router = crate::api::router(state).layer(axum::middleware::from_fn_with_state(
            pool,
            crate::auth::verify,
        ));
        App {
            router,
            js,
            _server: server,
            _storage: storage,
        }
    }

    fn multipart(parts: &[(&str, Option<&str>, &[u8])]) -> (String, Vec<u8>) {
        let boundary = "kanadetestboundary";
        let mut body = Vec::new();
        for (name, filename, data) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(f) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    async fn post(
        app: &App,
        uri: &str,
        filename: &str,
        file: &[u8],
        version: Option<&str>,
    ) -> (StatusCode, String) {
        let mut parts: Vec<(&str, Option<&str>, &[u8])> = Vec::new();
        if let Some(v) = version {
            parts.push(("version", None, v.as_bytes()));
        }
        parts.push(("file", Some(filename), file));
        let (ct, body) = multipart(&parts);
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {}", mint("op")))
            .header("x-kanade-source", "cli")
            .header("content-type", ct)
            .body(Body::from(body))
            .unwrap();
        let resp = app.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn publish_agent(
        app: &App,
        filename: &str,
        file: &[u8],
        version: Option<&str>,
    ) -> (StatusCode, String) {
        post(app, "/api/agents/publish", filename, file, version).await
    }

    /// `(key, size, digest, nuid)` of every object in `bucket`, sorted.
    async fn snapshot(app: &App, bucket: &str) -> Vec<(String, usize, Option<String>, String)> {
        let store = app.js.get_object_store(bucket).await.unwrap();
        let mut list = store.list().await.unwrap();
        let mut out = Vec::new();
        while let Some(info) = list.next().await {
            let i = info.unwrap();
            if !i.deleted {
                out.push((i.name, i.size, i.digest, i.nuid));
            }
        }
        out.sort();
        out
    }

    async fn read(app: &App, bucket: &str, key: &str) -> Vec<u8> {
        let store = app.js.get_object_store(bucket).await.unwrap();
        let mut obj = store.get(key).await.unwrap();
        let mut buf = Vec::new();
        obj.read_to_end(&mut buf).await.unwrap();
        buf
    }

    const AGENT: &str = "kanade-agent";

    fn linux_agent(version: &str) -> Vec<u8> {
        elf_x86_64(&[("", ".kanade", &meta(AGENT, version, "linux-x86_64"))])
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn official_agents_are_admitted_on_every_supported_platform() {
        let app = app().await;
        let mac = macho_arm64(&[(
            "__DATA",
            "__kanade",
            &meta(AGENT, "0.81.0", "macos-aarch64"),
        )]);
        let win = pe_x86_64(".kanade", &meta(AGENT, "0.81.0", "windows-x86_64"));
        let lin = linux_agent("0.81.0");

        // The file name is irrelevant; the version field is optional and may carry a `v`.
        for (name, bytes, version, key) in [
            ("whatever.bin", &lin, None, "0.81.0-linux-x86_64"),
            ("kanade-agent.exe", &win, Some("0.81.0"), "0.81.0"),
            ("kanade", &mac, Some("v0.81.0"), "0.81.0-macos-aarch64"),
        ] {
            let (st, body) = publish_agent(&app, name, bytes, version).await;
            assert_eq!(st, StatusCode::OK, "{key}: {body}");
            assert!(body.contains(&format!("\"key\":\"{key}\"")), "{body}");
            assert_eq!(&read(&app, OBJECT_AGENT_RELEASES, key).await, bytes);
        }
        assert!(snapshot(&app, OBJECT_APP_PACKAGES).await.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn rejected_uploads_leave_the_object_store_and_existing_release_untouched() {
        let app = app().await;
        let good = linux_agent("0.81.0");
        let (st, body) = publish_agent(&app, "kanade-agent", &good, Some("0.81.0")).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let before = snapshot(&app, OBJECT_AGENT_RELEASES).await;
        assert_eq!(before.len(), 1);

        let mac_cli = macho_arm64(&[(
            "__DATA",
            "__kanade",
            &meta("kanade", "0.81.0", "macos-aarch64"),
        )]);
        let malformed =
            elf_x86_64(&[("", ".kanade", b"KNAGMETA\x01\x00\x00\x00\xff\x00\x00\x00{")]);
        let mut elf_object = good.clone();
        elf_object[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        let mut mac_object = macho_arm64(&[(
            "__DATA",
            "__kanade",
            &meta(AGENT, "0.81.0", "macos-aarch64"),
        )]);
        mac_object[12..16].copy_from_slice(&1u32.to_le_bytes()); // MH_OBJECT
        let cases: Vec<(&str, Vec<u8>, Option<&str>, &str)> = vec![
            // Same key as the live release: an overwrite attempt per rejection kind.
            (
                "ELF intermediate object",
                elf_object,
                Some("0.81.0"),
                "linked kanade-agent executable",
            ),
            (
                "Mach-O intermediate object",
                mac_object,
                Some("0.81.0"),
                "linked kanade-agent executable",
            ),
            (
                "wrong product",
                elf_x86_64(&[("", ".kanade", &meta("kanade", "0.81.0", "linux-x86_64"))]),
                Some("0.81.0"),
                "'kanade', not 'kanade-agent'",
            ),
            (
                "backend",
                elf_x86_64(&[(
                    "",
                    ".kanade",
                    &meta("kanade-backend", "0.81.0", "linux-x86_64"),
                )]),
                None,
                "kanade-backend",
            ),
            ("renamed macOS CLI", mac_cli, Some("0.79.0"), "kanade-agent"),
            (
                "version mismatch",
                good.clone(),
                Some("0.80.0"),
                "version mismatch",
            ),
            (
                "platform mismatch",
                elf_x86_64(&[("", ".kanade", &meta(AGENT, "0.81.0", "macos-aarch64"))]),
                Some("0.81.0"),
                "platform",
            ),
            (
                "legacy, no metadata",
                elf_x86_64(&[("", ".rodata", b"kanade-agent 0.81.0")]),
                Some("0.81.0"),
                "no kanade-agent release metadata",
            ),
            ("malformed", malformed, Some("0.81.0"), "malformed"),
            (
                "not an executable",
                b"#!/bin/sh\n".to_vec(),
                Some("0.81.0"),
                "unrecognized",
            ),
        ];
        for (what, bytes, version, needle) in cases {
            let (st, body) = publish_agent(&app, "kanade-agent-0.81.0", &bytes, version).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{what}: {body}");
            assert!(body.contains(needle), "{what}: {body}");
            assert_eq!(
                snapshot(&app, OBJECT_AGENT_RELEASES).await,
                before,
                "{what}"
            );
        }
        assert_eq!(
            read(&app, OBJECT_AGENT_RELEASES, "0.81.0-linux-x86_64").await,
            good
        );

        // Identifying bytes appended to a legacy file do not make it an agent.
        let mut appended = elf_x86_64(&[("", ".rodata", b"x")]);
        appended.extend_from_slice(&meta(AGENT, "0.81.0", "linux-x86_64"));
        let (st, _) = publish_agent(&app, "a", &appended, Some("0.81.0")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(snapshot(&app, OBJECT_AGENT_RELEASES).await, before);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn duplicate_form_fields_are_rejected_before_any_write() {
        let app = app().await;
        let good = linux_agent("0.81.0");
        for parts in [
            vec![
                ("file", Some("a"), &good[..]),
                ("file", Some("b"), &good[..]),
            ],
            vec![
                ("version", None, &b"0.81.0"[..]),
                ("version", None, &b"0.81.0"[..]),
                ("file", Some("a"), &good[..]),
            ],
        ] {
            let (ct, body) = multipart(&parts);
            let req = Request::builder()
                .method("POST")
                .uri("/api/agents/publish")
                .header("authorization", format!("Bearer {}", mint("op")))
                .header("x-kanade-source", "cli")
                .header("content-type", ct)
                .body(Body::from(body))
                .unwrap();
            let resp = app.router.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
        assert!(snapshot(&app, OBJECT_AGENT_RELEASES).await.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn normal_app_publishing_still_accepts_any_file_and_never_touches_agent_releases() {
        let app = app().await;
        let mac_cli = macho_arm64(&[(
            "__DATA",
            "__kanade",
            &meta("kanade", "0.79.0", "macos-aarch64"),
        )]);
        let (st, body) = post(
            &app,
            "/api/app-packages/tool/1.0.0",
            "tool.bin",
            &mac_cli,
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(read(&app, OBJECT_APP_PACKAGES, "tool/1.0.0").await, mac_cli);
        // An arbitrary non-executable is fine as an app package too.
        let (st, body) = post(&app, "/api/app-packages/doc/2", "doc.txt", b"hello", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(snapshot(&app, OBJECT_AGENT_RELEASES).await.is_empty());
    }
}
