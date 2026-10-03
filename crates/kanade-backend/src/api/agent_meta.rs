//! Per-PC operator metadata (`agent_meta` KV bucket).
//!
//!   GET    /api/agents/{pc_id}/meta            (viewer+) -> AgentMeta
//!   PUT    /api/agents/{pc_id}/meta            (operator) replace the whole set
//!   PUT    /api/agents/{pc_id}/meta/key        (operator) upsert one key
//!   DELETE /api/agents/{pc_id}/meta/key?key=   (operator) remove one key
//!
//! Parallel to `agent_groups` membership: per-PC, operator-managed,
//! stored straight in JetStream KV (no SQLite projection — nothing on the
//! agent side reads it; the backend reads it on demand for the SPA). The
//! whole-set PUT re-normalises (trim / drop empty keys / dedup by key) via
//! [`AgentMeta::new`] so the stored JSON is stable regardless of input.
//!
//! The single-key routes exist for writers that must not disturb other
//! keys — the CLI (`kanade meta set` / `rm`) and the directory-sync job
//! that is the intended bulk producer. They do the read-modify-write here
//! as a compare-and-swap, and skip the write when nothing would change.
//! The key travels in the JSON body / query string rather than the path
//! because it is free-form (`/`, `?`, non-ASCII, surrounding spaces).
//! The whole-set PUT stays a blind put (the SPA's bulk edit), so it can
//! still overwrite keys a concurrent single-key writer just added; use the
//! single-key routes for anything automated. Clearing a PC is the
//! whole-set PUT with no entries.
//!
//! Every mutation records an audit event attributed to the caller. It is
//! published after the KV write succeeds and is best-effort (see
//! `audit::record`), so it is not atomic with the write.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use tracing::{info, warn};

use kanade_shared::kv::BUCKET_AGENT_META;
use kanade_shared::wire::{AgentMeta, MetaEntry, MetaUpdate};
use serde::Deserialize;

use super::AppState;
use crate::audit::{self, Caller};

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
    caller: Caller,
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
    audit::record(
        &state.nats,
        "operator",
        "agent_meta_put",
        Some(&pc_id),
        Some(&caller),
        serde_json::json!({ "count": normalised.entries.len() }),
    )
    .await;
    Ok(Json(normalised))
}

/// Query of the single-key remove route.
#[derive(Deserialize)]
pub struct KeyQuery {
    key: String,
}

fn empty_key() -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        "metadata key must not be empty".to_string(),
    )
}

/// `PUT /api/agents/{pc_id}/meta/key` — upsert one key, leaving every
/// other key untouched. An empty value keeps the key with a blank value.
pub async fn set_key(
    State(state): State<AppState>,
    caller: Caller,
    Path(pc_id): Path<String>,
    Json(body): Json<MetaEntry>,
) -> Result<Json<MetaUpdate>, (StatusCode, String)> {
    // Rejected before the CAS loop: it is a property of the request, not
    // something to rediscover on every retry.
    if body.key.trim().is_empty() {
        return Err(empty_key());
    }
    let kv = open_bucket(&state).await?;
    let update = update_key(&kv, &pc_id, |m| m.upsert(&body.key, &body.value)).await?;
    info!(pc_id = %pc_id, key = %body.key.trim(), changed = update.changed, "agent_meta set");
    audit::record(
        &state.nats,
        "operator",
        "agent_meta_set",
        Some(&pc_id),
        Some(&caller),
        serde_json::json!({
            "key": body.key.trim(),
            "value": body.value.trim(),
            "changed": update.changed,
        }),
    )
    .await;
    Ok(Json(update))
}

/// `DELETE /api/agents/{pc_id}/meta/key?key=…` — remove one key. An
/// absent key is not an error (`changed: false`).
pub async fn remove_key(
    State(state): State<AppState>,
    caller: Caller,
    Path(pc_id): Path<String>,
    Query(q): Query<KeyQuery>,
) -> Result<Json<MetaUpdate>, (StatusCode, String)> {
    if q.key.trim().is_empty() {
        return Err(empty_key());
    }
    let kv = open_bucket(&state).await?;
    let update = update_key(&kv, &pc_id, |m| m.remove(&q.key)).await?;
    info!(pc_id = %pc_id, key = %q.key.trim(), changed = update.changed, "agent_meta removed");
    audit::record(
        &state.nats,
        "operator",
        "agent_meta_rm",
        Some(&pc_id),
        Some(&caller),
        serde_json::json!({ "key": q.key.trim(), "changed": update.changed }),
    )
    .await;
    Ok(Json(update))
}

/// Apply `edit` (which reports whether it changed the set) to the PC's
/// attributes as a compare-and-swap read-modify-write, so a concurrent
/// writer of another key is never lost. An edit that changes nothing
/// skips the write entirely (no revision bump).
async fn update_key(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
    mut edit: impl FnMut(&mut AgentMeta) -> bool,
) -> Result<MetaUpdate, (StatusCode, String)> {
    let mut changed = false;
    let meta = kanade_shared::kv_cas::read_modify_write(kv, pc_id, |m: &mut AgentMeta| {
        // Overwritten on every CAS round; the last round is the one
        // that decided whether a write happened.
        changed = edit(m);
        changed
    })
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id, "agent_meta update");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update agent_meta for {pc_id}: {e:#}"),
        )
    })?;
    Ok(MetaUpdate { meta, changed })
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

#[cfg(test)]
mod tests {
    //! The single-key routes against a real JetStream KV through the real
    //! router. CAS conflicts and "unchanged ⇒ no revision bump" are
    //! properties of the broker, so a mock would only restate the
    //! implementation. Like the `agent_config` suite these are `#[ignore]`d
    //! (each spawns a throwaway `nats-server -js`, which must be in PATH):
    //!
    //! ```text
    //! cargo test -p kanade-backend agent_meta -- --ignored
    //! ```

    use std::process::Stdio;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    struct App {
        router: Router,
        nats: async_nats::Client,
        kv: async_nats::jetstream::kv::Store,
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

    /// Accounts: `op` (operator), `view` (viewer).
    async fn app() -> App {
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
        let nats = client.expect("nats-server did not come up in 5s");
        let js = async_nats::jetstream::new(nats.clone());
        let kv = js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: BUCKET_AGENT_META.to_string(),
                history: 5,
                ..Default::default()
            })
            .await
            .expect("create agent_meta bucket");

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for (name, role) in [("op", "operator"), ("view", "viewer")] {
            sqlx::query("INSERT INTO users (username, password_hash, role) VALUES (?, 'x', ?)")
                .bind(name)
                .bind(role)
                .execute(&pool)
                .await
                .unwrap();
        }
        let state = AppState {
            pool: pool.clone(),
            query_pool: pool.clone(),
            commands: std::sync::Arc::new(crate::command_publisher::CommandPublisher::new(
                nats.clone(),
                None,
            )),
            nats: nats.clone(),
            jetstream: js,
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
            nats,
            kv,
            _server: server,
            _storage: storage,
        }
    }

    async fn call(
        app: &App,
        method: &str,
        uri: &str,
        who: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {}", mint(who)))
            .header("x-kanade-source", "cli");
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = app
            .router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    const KEY_URL: &str = "/api/agents/PC-01/meta/key";

    async fn set(app: &App, who: &str, key: &str, value: &str) -> (StatusCode, String) {
        call(
            app,
            "PUT",
            KEY_URL,
            who,
            Some(serde_json::json!({ "key": key, "value": value })),
        )
        .await
    }

    fn update(body: &str) -> MetaUpdate {
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
    }

    async fn revision(app: &App) -> Option<u64> {
        app.kv.entry("PC-01").await.unwrap().map(|e| e.revision)
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn concurrent_upserts_of_different_keys_both_survive() {
        let app = app().await;
        let keys = ["name", "email", "dept", "note", "floor", "desk"];
        let mut tasks = Vec::new();
        for k in keys {
            let kv = app.kv.clone();
            tasks.push(tokio::spawn(async move {
                update_key(&kv, "PC-01", |m| m.upsert(k, "v"))
                    .await
                    .unwrap()
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().changed);
        }
        let (_, body) = call(&app, "GET", "/api/agents/PC-01/meta", "view", None).await;
        let meta: AgentMeta = serde_json::from_str(&body).unwrap();
        let mut got: Vec<_> = meta.entries.iter().map(|e| e.key.as_str()).collect();
        got.sort();
        let mut want = keys.to_vec();
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn unchanged_upsert_and_absent_rm_do_not_bump_the_revision() {
        let app = app().await;
        let (code, body) = set(&app, "op", "name", "Alice").await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(update(&body).changed);
        let rev = revision(&app).await;
        assert!(rev.is_some());

        // Same value (modulo surrounding whitespace): no write.
        let (_, body) = set(&app, "op", " name ", " Alice ").await;
        assert!(!update(&body).changed);
        assert_eq!(revision(&app).await, rev);

        // Absent key: success, no write.
        let (code, body) = call(&app, "DELETE", &format!("{KEY_URL}?key=ghost"), "op", None).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(!update(&body).changed);
        assert_eq!(revision(&app).await, rev);

        // A real change does bump it.
        let (_, body) = set(&app, "op", "name", "Bob").await;
        assert!(update(&body).changed);
        assert!(revision(&app).await > rev);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn empty_value_keeps_the_key_and_rm_drops_only_that_key() {
        let app = app().await;
        set(&app, "op", "note", "x").await;
        set(&app, "op", "other", "y").await;
        let (_, body) = set(&app, "op", "note", "").await;
        let u = update(&body);
        assert!(u.changed);
        assert_eq!(
            u.meta.entries,
            vec![MetaEntry::new("note", ""), MetaEntry::new("other", "y")]
        );
        let (_, body) = call(&app, "DELETE", &format!("{KEY_URL}?key=note"), "op", None).await;
        let u = update(&body);
        assert!(u.changed);
        assert_eq!(u.meta.entries, vec![MetaEntry::new("other", "y")]);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn empty_key_is_a_400_and_writes_nothing() {
        let app = app().await;
        let (code, body) = set(&app, "op", "  ", "v").await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body, "metadata key must not be empty");
        let (code, _) = call(&app, "DELETE", &format!("{KEY_URL}?key=%20"), "op", None).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(revision(&app).await, None);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn viewers_are_refused_and_nothing_is_written() {
        let app = app().await;
        let (code, _) = set(&app, "view", "k", "v").await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _) = call(&app, "DELETE", &format!("{KEY_URL}?key=k"), "view", None).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _) = call(
            &app,
            "PUT",
            "/api/agents/PC-01/meta",
            "view",
            Some(serde_json::json!({"entries": []})),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(revision(&app).await, None);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn free_form_keys_round_trip() {
        let app = app().await;
        for key in ["a/b", "what?", "部署", "a b&c=d+e"] {
            let (code, body) = set(&app, "op", key, "v").await;
            assert_eq!(code, StatusCode::OK, "{body}");
            let enc: String = url_encode(key);
            let (code, body) =
                call(&app, "DELETE", &format!("{KEY_URL}?key={enc}"), "op", None).await;
            assert_eq!(code, StatusCode::OK, "{body}");
            assert!(update(&body).changed, "{key}");
            assert!(update(&body).meta.entries.is_empty());
        }
    }

    fn url_encode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn put_empty_set_clears_all_keys() {
        let app = app().await;
        set(&app, "op", "a", "1").await;
        let (code, _) = call(
            &app,
            "PUT",
            "/api/agents/PC-01/meta",
            "op",
            Some(serde_json::json!({"entries": []})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let (_, body) = call(&app, "GET", "/api/agents/PC-01/meta", "view", None).await;
        assert!(serde_json::from_str::<AgentMeta>(&body).unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn every_mutation_is_audited_under_the_callers_account() {
        use futures::StreamExt;
        let app = app().await;
        let mut sub = app.nats.subscribe("audit.>").await.unwrap();
        app.nats.flush().await.unwrap();

        set(&app, "op", "name", "Alice").await;
        set(&app, "op", "name", "Alice").await; // no-op, still audited
        call(&app, "DELETE", &format!("{KEY_URL}?key=name"), "op", None).await;
        call(
            &app,
            "PUT",
            "/api/agents/PC-01/meta",
            "op",
            Some(serde_json::json!({"entries": []})),
        )
        .await;

        let expected = [
            ("audit.operator.agent_meta_set.PC-01", Some(true)),
            ("audit.operator.agent_meta_set.PC-01", Some(false)),
            ("audit.operator.agent_meta_rm.PC-01", Some(true)),
            ("audit.operator.agent_meta_put.PC-01", None),
        ];
        for (subject, changed) in expected {
            let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
                .await
                .expect("audit event")
                .expect("subscription open");
            assert_eq!(msg.subject.as_str(), subject);
            let v: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
            assert_eq!(v["payload"]["sub"], "op", "{v}");
            assert_eq!(v["payload"]["source"], "cli", "{v}");
            if let Some(c) = changed {
                assert_eq!(v["payload"]["changed"], c, "{v}");
            }
        }
    }
}
