//! RBAC account management: credential login (mints a short-lived
//! HS256 JWT), self-service password change, and admin-only CRUD over
//! the SQLite `users` table.
//!
//! Role enforcement for the CRUD routes is applied at the router level
//! (`route_layer(require_admin)` in [`crate::api::router`]); the
//! handlers here assume the caller already cleared that gate. The
//! login route is public (allow-listed in [`crate::auth::verify`]).

use anyhow::Context as _;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::warn;
use uuid::Uuid;

use crate::api::AppState;
use crate::api::password_setup::{self, PURPOSE_RESET, PURPOSE_SETUP};
use crate::audit::{self, Caller};
use crate::auth::{Claims, EXPECTED_AUDIENCE, Role, signing_secret};
use kanade_shared::feature::Feature;

// ---- allowed_features (page allow-list) helpers --------------------

/// Validate + canonicalize a submitted page allow-list, mapping an unknown
/// key to a `400`. Thin wrapper over the shared [`Feature::canonicalize`]
/// (single source of truth, also used by the permission-group editor) that
/// adapts its `Err(bad_key)` to this module's small `(status, message)` error
/// — a full `Response` here would trip `clippy::result_large_err` on a
/// non-async helper; call sites convert it via [`err`].
fn canonical_features(keys: &[String]) -> Result<Vec<String>, (StatusCode, String)> {
    Feature::canonicalize(keys)
        .map_err(|k| (StatusCode::BAD_REQUEST, format!("unknown feature: {k}")))
}

/// Serialize a validated allow-list to the JSON TEXT stored in
/// `users.allowed_features`. Same small-`Err` convention as
/// [`canonical_features`].
fn features_to_json(keys: &[String]) -> Result<String, (StatusCode, String)> {
    let canon = canonical_features(keys)?;
    serde_json::to_string(&canon).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "serialize features".into(),
        )
    })
}

/// Convert the small helper error into the handler's `Response` error.
fn feature_err((code, msg): (StatusCode, String)) -> Response {
    err(code, &msg)
}

/// Lenient read of the stored allow-list for display: `NULL`/malformed →
/// `None` (unrestricted), unknown/retired keys dropped. Mirrors
/// `auth::parse_allowed_features` so the admin list and the enforcement path
/// agree on what a stored value means.
fn features_for_display(raw: Option<&str>) -> Option<Vec<String>> {
    let keys: Vec<String> = serde_json::from_str(raw?).ok()?;
    Some(
        keys.iter()
            .filter_map(|k| Feature::parse(k).map(|f| f.as_str().to_string()))
            .collect(),
    )
}

/// `serde` "double option" for a `PATCH` field that must distinguish three
/// wire states: **absent** (leave unchanged → `None`), explicit **`null`**
/// (clear → `Some(None)`), and a **value** (`Some(Some(v))`). Plain
/// `Option<Option<T>>` can't: serde folds `null` into the outer `None`, same
/// as absent. `deserialize_with` is only invoked when the key is present, so
/// routing through it recovers the distinction.
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(de)?))
}

// Token lifetime is operator-configurable via `server_settings`
// (`session_ttl_hours`, default 24h — see
// `kanade_shared::wire::ServerSettings`). The DB row is re-checked on every
// request (see [`crate::auth::verify`]), so this window only bounds how
// long a token stays valid after the user record is *deleted* — `disable`
// takes effect immediately regardless.
/// Shared with [`crate::api::password_setup`] so the link-based set/reset
/// path enforces the same minimum as login-managed changes.
pub(crate) const MIN_PASSWORD_LEN: usize = 8;

const REG_SUBKEY: &str = r"SOFTWARE\kanade\backend";
const REG_BOOTSTRAP_PW: &str = "BootstrapAdminPassword";
const ENV_BOOTSTRAP_PW: &str = "KANADE_BOOTSTRAP_ADMIN_PASSWORD";
const ENV_BOOTSTRAP_USER: &str = "KANADE_BOOTSTRAP_ADMIN_USER";

// ---- password hashing (argon2id) ----------------------------------

fn hash_password(pw: &str) -> anyhow::Result<String> {
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    let hash = Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hash: {e}"))?
        .to_string();
    Ok(hash)
}

fn verify_password(pw: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// #504: Argon2id is tens-to-hundreds of ms of pure CPU. The handlers
// below run on the same tokio runtime as the projectors and the
// scheduler, so hashing inline blocked a worker per attempt — and
// `login` is public, so a dumb brute force coupled auth load
// directly to fleet-pipeline latency on the mini PC's few cores.
// spawn_blocking moves the work to the blocking pool.

pub(crate) async fn hash_password_async(pw: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || hash_password(&pw))
        .await
        .map_err(|e| anyhow::anyhow!("hash task join: {e}"))?
}

async fn verify_password_async(pw: String, phc: String) -> bool {
    // A join error means the verify closure panicked. Fail closed
    // (treat as a failed verification), but log it — otherwise a
    // panicking verifier is indistinguishable from a wrong password
    // in production logs.
    match tokio::task::spawn_blocking(move || verify_password(&pw, &phc)).await {
        Ok(ok) => ok,
        Err(e) => {
            warn!(error = %e, "password verify task panicked; failing closed");
            false
        }
    }
}

// ---- JWT minting ---------------------------------------------------

/// Mint a signed HS256 token valid for `ttl_hours` hours. `None` on a
/// signing failure (logged) — the caller maps that to a 500. `ttl_hours`
/// is the operator-configured session window, already clamped to a sane
/// range by [`kanade_shared::wire::ServerSettings::effective_session_ttl_hours`].
fn mint_jwt(sub: &str, role: Role, ttl_hours: i64) -> Option<(String, i64)> {
    let exp = (Utc::now() + Duration::hours(ttl_hours)).timestamp();
    let claims = Claims {
        sub: sub.to_string(),
        exp,
        aud: Some(EXPECTED_AUDIENCE.to_string()),
        roles: vec![role.as_str().to_string()],
        // Never minted into the token — the page allow-list is resolved from
        // the DB on every request (see `auth::verify`), like `roles`.
        allowed_features: None,
    };
    let secret = signing_secret();
    match encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    ) {
        Ok(token) => Some((token, exp)),
        Err(e) => {
            warn!(error = %e, "JWT mint failed");
            None
        }
    }
}

// ---- error helper --------------------------------------------------

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_owned()).into_response()
}

// ---- login / me / change-password ----------------------------------

#[derive(Deserialize)]
pub struct LoginReq {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResp {
    token: String,
    role: Role,
    must_change_pw: bool,
    exp: i64,
}

/// `POST /api/auth/login` — public. Verifies credentials and mints a
/// JWT. Returns `401` for unknown user / bad password / disabled
/// account (deliberately indistinguishable to the caller).
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, Response> {
    let row = sqlx::query_as::<_, (String, String, i64, i64)>(
        "SELECT password_hash, role, disabled, must_change_pw FROM users WHERE username = ?",
    )
    .bind(&req.username)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "login query failed");
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth backend unavailable",
        )
    })?;

    let unauthorized = || err(StatusCode::UNAUTHORIZED, "invalid credentials");
    let Some((hash, role, disabled, must_change_pw)) = row else {
        return Err(unauthorized());
    };
    if disabled != 0 || !verify_password_async(req.password.clone(), hash).await {
        return Err(unauthorized());
    }
    let Some(role) = Role::parse(&role) else {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "corrupt role"));
    };

    // Operator-configured session window (default 24h). A broker hiccup
    // reading the KV must not block login — fall back to the built-in
    // default rather than 500ing an otherwise-valid sign-in. Log the error
    // (mirroring the sqlx failure above) so a persistent KV/broker outage is
    // observable rather than silently degrading every login to the default.
    let ttl_hours = match super::server_settings::load(&state).await {
        Ok(s) => s.effective_session_ttl_hours(),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "server_settings load failed at login; using default session TTL");
            kanade_shared::wire::DEFAULT_SESSION_TTL_HOURS
        }
    } as i64;
    let (token, exp) = mint_jwt(&req.username, role, ttl_hours)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "token mint failed"))?;
    Ok(Json(LoginResp {
        token,
        role,
        must_change_pw: must_change_pw != 0,
        exp,
    }))
}

#[derive(Serialize)]
pub struct MeResp {
    username: String,
    role: Role,
    must_change_pw: bool,
    /// The caller's page allow-list (`None` = unrestricted — every page).
    /// Resolved from the DB by [`crate::auth::verify`]; drives the SPA's
    /// sidebar + route filtering (the hard backend gate is `require_features`).
    allowed_features: Option<Vec<Feature>>,
}

/// `GET /api/auth/me` — the caller's own identity + effective role.
/// Drives the SPA's UI gating (hide operator/admin actions) and the
/// forced password-change gate (`must_change_pw`). The flag is read live
/// from the DB so it clears as soon as the user changes their password.
pub async fn me(
    State(state): State<AppState>,
    claims: axum::Extension<Claims>,
) -> Result<Json<MeResp>, Response> {
    // Service tokens have no `users` row; treat them as never needing a
    // password change.
    let must_change_pw =
        sqlx::query_scalar::<_, i64>("SELECT must_change_pw FROM users WHERE username = ?")
            .bind(&claims.sub)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_err)?
            .unwrap_or(0);
    Ok(Json(MeResp {
        username: claims.sub.clone(),
        role: claims.role(),
        must_change_pw: must_change_pw != 0,
        // Already resolved from the DB by `verify` — no extra query needed.
        allowed_features: claims.allowed_features.clone(),
    }))
}

#[derive(Deserialize)]
pub struct ChangePwReq {
    old_password: String,
    new_password: String,
}

/// `POST /api/auth/change-password` — self-service. Verifies the old
/// password, stores the new one, clears `must_change_pw`.
pub async fn change_password(
    State(state): State<AppState>,
    claims: axum::Extension<Claims>,
    caller: Caller,
    Json(req): Json<ChangePwReq>,
) -> Result<StatusCode, Response> {
    if req.new_password.chars().count() < MIN_PASSWORD_LEN {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "new password too short (min 8 chars)",
        ));
    }
    let username = claims.sub.clone();
    let hash =
        sqlx::query_scalar::<_, String>("SELECT password_hash FROM users WHERE username = ?")
            .bind(&username)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_err)?
            .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "unknown account"))?;
    if !verify_password_async(req.old_password.clone(), hash).await {
        return Err(err(StatusCode::UNAUTHORIZED, "old password incorrect"));
    }
    let new_hash = hash_password_async(req.new_password.clone())
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "hash failed"))?;
    sqlx::query(
        "UPDATE users SET password_hash = ?, must_change_pw = 0, updated_at = CURRENT_TIMESTAMP WHERE username = ?",
    )
    .bind(&new_hash)
    .bind(&username)
    .execute(&state.pool)
    .await
    .map_err(db_err)?;

    audit::record(
        &state.nats,
        "admin",
        "account.change_password",
        Some(&username),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// ---- admin CRUD ----------------------------------------------------

/// Raw `users` row as stored (JSON `allowed_features` kept as TEXT so the
/// handler can parse it leniently — the DB has no JSON type).
#[derive(sqlx::FromRow)]
struct UserRowDb {
    username: String,
    role: String,
    disabled: i64,
    must_change_pw: i64,
    email: Option<String>,
    allowed_features: Option<String>,
    permission_group: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize)]
pub struct UserRow {
    username: String,
    role: String,
    disabled: i64,
    must_change_pw: i64,
    /// Optional contact email (#770) — drives the SPA's email column and
    /// the "send setup/reset link" action. `None` when unset.
    email: Option<String>,
    /// The account's own page allow-list (`kanade_shared::feature::Feature`
    /// keys). `None` = unrestricted. Ignored at enforcement time when
    /// `permission_group` is set (the group wins — see `auth::lookup_user`),
    /// but still returned so the SPA can show it when no group is assigned.
    allowed_features: Option<Vec<String>>,
    /// #1008 Phase 3: the permission group this account belongs to, or `None`.
    /// When set, the group's feature set is the account's effective access.
    permission_group: Option<String>,
    created_at: String,
    updated_at: String,
}

/// `GET /api/accounts` — admin. Never returns password hashes.
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<UserRow>>, Response> {
    let rows = sqlx::query_as::<_, UserRowDb>(
        "SELECT username, role, disabled, must_change_pw, email, allowed_features, permission_group, created_at, updated_at FROM users ORDER BY username",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(db_err)?;
    let out = rows
        .into_iter()
        .map(|r| UserRow {
            username: r.username,
            role: r.role,
            disabled: r.disabled,
            must_change_pw: r.must_change_pw,
            email: r.email,
            allowed_features: features_for_display(r.allowed_features.as_deref()),
            permission_group: r.permission_group,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Ok(Json(out))
}

/// 404-guard helper: a `permission_group` an account is being assigned must
/// exist. Returns `Ok(())` for `None` (no assignment). `400` when the named
/// group is unknown, so a typo can't silently leave the account unrestricted
/// (a dangling group falls back to the per-user list in `auth`).
async fn ensure_group_exists(pool: &sqlx::SqlitePool, group: Option<&str>) -> Result<(), Response> {
    let Some(group) = group else { return Ok(()) };
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM permission_groups WHERE name = ?")
        .bind(group)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
    if exists == 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            &format!("no such permission group: {group}"),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateReq {
    username: String,
    /// Optional (#770): when omitted, an `email` must be given and the
    /// user sets their own password via the emailed setup link.
    #[serde(default)]
    password: Option<String>,
    role: String,
    /// Optional contact email. When present without a `password`, the
    /// account is created with an unusable password and a one-time setup
    /// link is mailed.
    #[serde(default)]
    email: Option<String>,
    /// Optional page allow-list. Omitted / `null` → unrestricted (every
    /// page). An array (possibly empty) restricts the account. Unknown keys
    /// are rejected with `400`.
    #[serde(default)]
    allowed_features: Option<Vec<String>>,
    /// Optional #1008 Phase 3 permission group. When set, the group governs
    /// the account's page access (overriding `allowed_features`). `400` if the
    /// named group doesn't exist.
    #[serde(default)]
    permission_group: Option<String>,
}

#[derive(Serialize)]
pub struct CreateResp {
    /// True when the account was created via the email-link path and a
    /// setup link was issued/sent (so the SPA can tell the admin the user
    /// will receive a link rather than needing a password handed over).
    setup_link_sent: bool,
}

/// `POST /api/accounts` — admin. Creates a user; `409` on duplicate.
///
/// Two paths:
///  - **password given** → classic create (email, if any, is stored;
///    no link sent).
///  - **email given, no password** → create with an unusable random hash
///    and mail a one-time setup link (requires `[mail]` configured).
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Json(req): Json<CreateReq>,
) -> Result<(StatusCode, Json<CreateResp>), Response> {
    let username = req.username.trim();
    if username.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "username required"));
    }
    let Some(role) = Role::parse(&req.role) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "role must be viewer/operator/admin",
        ));
    };
    let email = req
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());
    if let Some(e) = email
        && e.parse::<lettre::Address>().is_err()
    {
        return Err(err(StatusCode::BAD_REQUEST, "invalid email address"));
    }
    let password = req.password.as_deref().filter(|p| !p.is_empty());

    // Validate the allow-list up front (before hashing) so an unknown key
    // fails fast. `None` → stored NULL = unrestricted.
    let allowed_json: Option<String> = match &req.allowed_features {
        Some(keys) => Some(features_to_json(keys).map_err(feature_err)?),
        None => None,
    };
    // A named group must exist (else `400`). Trimmed like the update path so
    // whitespace can't sneak in a name that won't match at join time.
    let permission_group = req
        .permission_group
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty());
    ensure_group_exists(&state.pool, permission_group).await?;

    // Decide the path. The link path needs a mailer; the no-credential
    // case (neither password nor email) is rejected.
    let use_link = match (password, email) {
        (Some(pw), _) => {
            if pw.chars().count() < MIN_PASSWORD_LEN {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "password too short (min 8 chars)",
                ));
            }
            false
        }
        (None, Some(_)) => {
            if state.mailer.is_none() {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "[mail] not configured — set a password instead of emailing a setup link",
                ));
            }
            true
        }
        (None, None) => {
            return Err(err(StatusCode::BAD_REQUEST, "password or email required"));
        }
    };

    // The link path stores an unguessable random hash so the account
    // can't be logged into until the user sets a password via the link.
    let hash = match password {
        Some(pw) => hash_password_async(pw.to_owned()),
        None => hash_password_async(Uuid::new_v4().to_string()),
    }
    .await
    .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "hash failed"))?;

    let res = sqlx::query(
        "INSERT INTO users (username, password_hash, role, must_change_pw, email, allowed_features, permission_group) \
         VALUES (?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(username)
    .bind(&hash)
    .bind(role.as_str())
    .bind(email)
    .bind(allowed_json.as_deref())
    .bind(permission_group)
    .execute(&state.pool)
    .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "username already exists"));
        }
        Err(e) => return Err(db_err(e)),
    }

    // Email path: issue + mail the setup link (best-effort). The account
    // already exists; if the link can't be sent (no Host/public_url) the
    // admin can re-send via `reset_link`.
    let mut setup_link_sent = false;
    if use_link
        && let Some(email) = email
        && let Some(mailer) = &state.mailer
    {
        match password_setup::link_base(state.public_url.as_deref(), &headers) {
            Some(base) => {
                match password_setup::issue_token(&state.pool, username, PURPOSE_SETUP).await {
                    // Reflect the real SMTP outcome, not just "a token was
                    // made", so the admin UI doesn't claim a link went out
                    // when delivery actually failed.
                    Ok(raw) => {
                        setup_link_sent =
                            password_setup::send_link(mailer, &base, email, &raw, PURPOSE_SETUP)
                                .await;
                    }
                    Err(e) => warn!(error = %e, username, "create: issue setup token"),
                }
            }
            None => warn!(
                username,
                "create: no link base (Host/public_url) — setup link not sent"
            ),
        }
    }

    audit::record(
        &state.nats,
        "admin",
        "account.create",
        Some(username),
        Some(&caller),
        serde_json::json!({
            "role": role.as_str(),
            "has_email": email.is_some(),
            "setup_link_sent": setup_link_sent,
            "restricted": allowed_json.is_some(),
            "permission_group": permission_group,
        }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(CreateResp { setup_link_sent })))
}

/// `POST /api/accounts/{username}/reset-link` — admin. Mails a one-time
/// password-reset link to the account's stored email. `409` when the
/// account has no email on file; `400` when `[mail]` is unconfigured.
pub async fn reset_link(
    State(state): State<AppState>,
    Path(username): Path<String>,
    caller: Caller,
    headers: HeaderMap,
) -> Result<StatusCode, Response> {
    let Some(mailer) = &state.mailer else {
        return Err(err(StatusCode::BAD_REQUEST, "[mail] not configured"));
    };
    let Some(email) = password_setup::email_for_user(&state.pool, &username)
        .await
        .map_err(db_err)?
    else {
        return Err(err(StatusCode::CONFLICT, "account has no email on file"));
    };
    let Some(base) = password_setup::link_base(state.public_url.as_deref(), &headers) else {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot determine link base URL (set [server] public_url)",
        ));
    };
    let raw = password_setup::issue_token(&state.pool, &username, PURPOSE_RESET)
        .await
        .map_err(db_err)?;
    let delivered = password_setup::send_link(mailer, &base, &email, &raw, PURPOSE_RESET).await;

    audit::record(
        &state.nats,
        "admin",
        "account.reset_link",
        Some(&username),
        Some(&caller),
        serde_json::json!({ "delivered": delivered }),
    )
    .await;
    // Surface a failed send honestly — the token was issued but the email
    // didn't go out, so the admin should retry / check SMTP rather than
    // assume the user got a link.
    if delivered {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(err(
            StatusCode::BAD_GATEWAY,
            "link generated but email delivery failed — check [mail] / SMTP",
        ))
    }
}

#[derive(Deserialize)]
pub struct UpdateReq {
    role: Option<String>,
    password: Option<String>,
    disabled: Option<bool>,
    /// Set/clear the contact email (#770). `Some("")` (or whitespace)
    /// clears it; `None` leaves it unchanged. Storing only — never sends.
    #[serde(default)]
    email: Option<String>,
    /// Set/clear the page allow-list. Absent → unchanged; `null` → clear
    /// (unrestricted); array (possibly empty) → restrict to those pages.
    /// The double-option decoder recovers the absent-vs-null distinction
    /// that plain `Option<Option<_>>` would lose.
    #[serde(default, deserialize_with = "double_option")]
    allowed_features: Option<Option<Vec<String>>>,
    /// Set/clear the #1008 Phase 3 permission group. Absent → unchanged;
    /// `null` → clear (fall back to `allowed_features`); a name → assign (must
    /// exist, else `400`). Same double-option tri-state as `allowed_features`.
    #[serde(default, deserialize_with = "double_option")]
    permission_group: Option<Option<String>>,
}

/// `PATCH /api/accounts/{username}` — admin. Any subset of role /
/// password / disabled. Guards the last enabled admin against demotion
/// or disable so the fleet can't lock itself out.
pub async fn update(
    State(state): State<AppState>,
    Path(username): Path<String>,
    caller: Caller,
    Json(req): Json<UpdateReq>,
) -> Result<StatusCode, Response> {
    // Validate every field up front so a bad input can't leave a
    // partially-applied update behind.
    let new_role = match &req.role {
        Some(r) => Some(Role::parse(r).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "role must be viewer/operator/admin",
            )
        })?),
        None => None,
    };
    if let Some(password) = &req.password
        && password.chars().count() < MIN_PASSWORD_LEN
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "password too short (min 8 chars)",
        ));
    }
    // Validate email up front (empty = clear).
    let new_email: Option<Option<&str>> = match &req.email {
        None => None,
        Some(e) => {
            let t = e.trim();
            if t.is_empty() {
                Some(None)
            } else {
                if t.parse::<lettre::Address>().is_err() {
                    return Err(err(StatusCode::BAD_REQUEST, "invalid email address"));
                }
                Some(Some(t))
            }
        }
    };

    // Validate the allow-list up front. `None` = unchanged; `Some(None)` =
    // clear to NULL (unrestricted); `Some(Some(json))` = set to that list.
    let new_allowed: Option<Option<String>> = match &req.allowed_features {
        None => None,
        Some(None) => Some(None),
        Some(Some(keys)) => Some(Some(features_to_json(keys).map_err(feature_err)?)),
    };

    // Validate the permission group up front. `None` = unchanged; `Some(None)`
    // = clear; `Some(Some(name))` = assign (an empty/whitespace name clears,
    // and a non-empty one must reference an existing group).
    let new_group: Option<Option<&str>> = match &req.permission_group {
        None => None,
        Some(None) => Some(None),
        Some(Some(name)) => {
            let t = name.trim();
            if t.is_empty() {
                Some(None)
            } else {
                ensure_group_exists(&state.pool, Some(t)).await?;
                Some(Some(t))
            }
        }
    };

    // 404 if the account is gone. The last-admin guards below live
    // *inside* each mutating statement (a `NOT (… AND
    // (SELECT COUNT(admins)) <= 1)` predicate), so the count and the
    // write are evaluated atomically — no check-then-act race
    // (Gemini #331). `rows_affected == 0` on a confirmed-existing row
    // therefore means the guard fired.
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = ?")
        .bind(&username)
        .fetch_one(&state.pool)
        .await
        .map_err(db_err)?;
    if exists == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }

    if let Some(role) = new_role {
        let res = sqlx::query(
            "UPDATE users SET role = ?1, updated_at = CURRENT_TIMESTAMP \
             WHERE username = ?2 \
               AND NOT (role = 'admin' AND disabled = 0 AND ?1 <> 'admin' \
                        AND (SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0) <= 1)",
        )
        .bind(role.as_str())
        .bind(&username)
        .execute(&state.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(err(
                StatusCode::CONFLICT,
                "cannot demote the last enabled admin",
            ));
        }
    }
    if let Some(disabled) = req.disabled {
        let res = sqlx::query(
            "UPDATE users SET disabled = ?1, updated_at = CURRENT_TIMESTAMP \
             WHERE username = ?2 \
               AND NOT (?1 = 1 AND role = 'admin' AND disabled = 0 \
                        AND (SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0) <= 1)",
        )
        .bind(disabled as i64)
        .bind(&username)
        .execute(&state.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(err(
                StatusCode::CONFLICT,
                "cannot disable the last enabled admin",
            ));
        }
    }
    if let Some(password) = &req.password {
        let hash = hash_password_async(password.clone())
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "hash failed"))?;
        // A reset forces the user to choose a new password on next login.
        sqlx::query(
            "UPDATE users SET password_hash = ?, must_change_pw = 1, updated_at = CURRENT_TIMESTAMP WHERE username = ?",
        )
        .bind(&hash)
        .bind(&username)
        .execute(&state.pool)
        .await
        .map_err(db_err)?;
    }
    if let Some(email) = new_email {
        sqlx::query(
            "UPDATE users SET email = ?, updated_at = CURRENT_TIMESTAMP WHERE username = ?",
        )
        .bind(email)
        .bind(&username)
        .execute(&state.pool)
        .await
        .map_err(db_err)?;
    }
    if let Some(allowed) = new_allowed {
        // `allowed` is `Option<String>`: `None` binds SQL NULL (unrestricted),
        // `Some(json)` binds the validated allow-list. The change takes effect
        // on the account's next request — `verify` re-reads the row every time.
        sqlx::query(
            "UPDATE users SET allowed_features = ?, updated_at = CURRENT_TIMESTAMP WHERE username = ?",
        )
        .bind(allowed.as_deref())
        .bind(&username)
        .execute(&state.pool)
        .await
        .map_err(db_err)?;
    }
    if let Some(group) = new_group {
        // `group` is `Option<&str>`: `None` binds SQL NULL (leave the group,
        // fall back to `allowed_features`), `Some(name)` assigns the group.
        sqlx::query(
            "UPDATE users SET permission_group = ?, updated_at = CURRENT_TIMESTAMP WHERE username = ?",
        )
        .bind(group)
        .bind(&username)
        .execute(&state.pool)
        .await
        .map_err(db_err)?;
    }

    audit::record(
        &state.nats,
        "admin",
        "account.update",
        Some(&username),
        Some(&caller),
        serde_json::json!({
            "role": req.role,
            "disabled": req.disabled,
            "password_reset": req.password.is_some(),
            "email_changed": new_email.is_some(),
            "allowed_features_changed": req.allowed_features.is_some(),
            "permission_group_changed": req.permission_group.is_some(),
        }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/accounts/{username}` — admin. Guards the last enabled
/// admin.
pub async fn delete(
    State(state): State<AppState>,
    Path(username): Path<String>,
    caller: Caller,
) -> Result<StatusCode, Response> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = ?")
        .bind(&username)
        .fetch_one(&state.pool)
        .await
        .map_err(db_err)?;
    if exists == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    // Last-admin guard enforced inside the statement so the count and
    // the delete are atomic (see `update`).
    let res = sqlx::query(
        "DELETE FROM users WHERE username = ? \
           AND NOT (role = 'admin' AND disabled = 0 \
                    AND (SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0) <= 1)",
    )
    .bind(&username)
    .execute(&state.pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(err(
            StatusCode::CONFLICT,
            "cannot delete the last enabled admin",
        ));
    }
    // Clean up any outstanding setup/reset token explicitly: the FK's
    // `ON DELETE CASCADE` is a no-op because the pool doesn't run with
    // `PRAGMA foreign_keys = ON` (enabling it globally would change
    // behaviour for the whole schema), so without this a deleted user
    // would leave an orphaned token row behind.
    if let Err(e) = sqlx::query("DELETE FROM password_setup_tokens WHERE username = ?")
        .bind(&username)
        .execute(&state.pool)
        .await
    {
        warn!(error = %e, %username, "delete: failed to clear password setup token");
    }

    audit::record(
        &state.nats,
        "admin",
        "account.delete",
        Some(&username),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

fn db_err(e: sqlx::Error) -> Response {
    warn!(error = %e, "accounts db error");
    err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
}

// ---- bootstrap seed (called from main on startup) ------------------

/// Seed the first admin when the `users` table is empty. Password is
/// resolved registry-first (`BootstrapAdminPassword`) / env-second
/// (`$KANADE_BOOTSTRAP_ADMIN_PASSWORD`); username from
/// `$KANADE_BOOTSTRAP_ADMIN_USER` (default `admin`). The seeded account
/// has `must_change_pw=1`. No-op (with a loud warning) when no password
/// is configured.
pub async fn seed_bootstrap_admin(pool: &SqlitePool) -> anyhow::Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .context("count users")?;
    if count > 0 {
        return Ok(());
    }

    let username = std::env::var(ENV_BOOTSTRAP_USER)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "admin".to_string());
    let password =
        kanade_shared::secrets::read_hklm_value(REG_SUBKEY, REG_BOOTSTRAP_PW).or_else(|| {
            std::env::var(ENV_BOOTSTRAP_PW)
                .ok()
                .filter(|s| !s.is_empty())
        });
    let Some(password) = password else {
        warn!(
            "no users and no bootstrap admin password (registry {REG_BOOTSTRAP_PW} / ${ENV_BOOTSTRAP_PW}) — no admin seeded. Set one and restart, or use the static service token."
        );
        return Ok(());
    };

    let hash = hash_password(&password).context("hash bootstrap password")?;
    sqlx::query(
        "INSERT INTO users (username, password_hash, role, must_change_pw) VALUES (?, ?, 'admin', 1)",
    )
    .bind(&username)
    .bind(&hash)
    .execute(pool)
    .await
    .context("insert bootstrap admin")?;
    warn!(%username, "seeded bootstrap admin — change the password on first login");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_features_validates_dedupes_orders() {
        // Unknown key → rejected.
        assert!(canonical_features(&["bogus".into()]).is_err());
        // Dedup + catalog order (input scrambled/duplicated).
        let got =
            canonical_features(&["compliance".into(), "inventory".into(), "compliance".into()])
                .unwrap();
        // Catalog order (Inventory precedes Compliance in `Feature::ALL`),
        // not input order, and de-duplicated.
        assert_eq!(got, vec!["inventory".to_string(), "compliance".to_string()]);
        // Empty is valid (commons-only restriction).
        assert_eq!(canonical_features(&[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn features_for_display_is_lenient() {
        assert_eq!(features_for_display(None), None);
        assert_eq!(features_for_display(Some("garbage")), None); // malformed → unrestricted
        assert_eq!(
            features_for_display(Some(r#"["audit","gone"]"#)),
            Some(vec!["audit".to_string()]) // unknown dropped
        );
        assert_eq!(features_for_display(Some("[]")), Some(vec![]));
    }

    #[test]
    fn update_req_double_option_tristate() {
        // Absent → None (leave unchanged).
        let absent: UpdateReq = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.allowed_features, None);
        // Explicit null → Some(None) (clear → unrestricted).
        let cleared: UpdateReq = serde_json::from_str(r#"{"allowed_features":null}"#).unwrap();
        assert_eq!(cleared.allowed_features, Some(None));
        // Array → Some(Some(list)) (restrict).
        let set: UpdateReq = serde_json::from_str(r#"{"allowed_features":["audit"]}"#).unwrap();
        assert_eq!(set.allowed_features, Some(Some(vec!["audit".to_string()])));
    }

    #[test]
    fn password_hash_roundtrip() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn min_password_len_counts_chars_not_bytes() {
        // A 6-character all-ASCII string is too short; a 6-character
        // multibyte string must ALSO be treated as 6 (Gemini #331):
        // byte-length would have wrongly passed it as 18.
        assert!("abcdef".chars().count() < MIN_PASSWORD_LEN);
        let jp = "ぱすわーど"; // 5 chars, 15 bytes
        assert_eq!(jp.chars().count(), 5);
        assert!(jp.chars().count() < MIN_PASSWORD_LEN);
        assert!(jp.len() > MIN_PASSWORD_LEN); // byte-length would mislead
    }

    async fn mem_pool_with_admins(n: usize) -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for i in 0..n {
            sqlx::query(
                "INSERT INTO users (username, password_hash, role) VALUES (?, 'x', 'admin')",
            )
            .bind(format!("admin{i}"))
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    // The guarded statements below mirror the predicates used by the
    // `update` / `delete` handlers. They prove the count + write are
    // atomic per statement so the fleet can't be left with zero admins.
    const GUARDED_DELETE: &str = "DELETE FROM users WHERE username = ? \
        AND NOT (role = 'admin' AND disabled = 0 \
                 AND (SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0) <= 1)";

    #[tokio::test]
    async fn last_admin_delete_is_blocked() {
        let pool = mem_pool_with_admins(1).await;
        let res = sqlx::query(GUARDED_DELETE)
            .bind("admin0")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(res.rows_affected(), 0, "sole admin must not be deletable");
    }

    #[tokio::test]
    async fn non_last_admin_delete_succeeds() {
        let pool = mem_pool_with_admins(2).await;
        let res = sqlx::query(GUARDED_DELETE)
            .bind("admin0")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(res.rows_affected(), 1, "one of two admins is deletable");
    }
}
