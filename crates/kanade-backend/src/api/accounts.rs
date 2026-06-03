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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::warn;

use crate::api::AppState;
use crate::audit::{self, Caller};
use crate::auth::{Claims, EXPECTED_AUDIENCE, Role, signing_secret};

/// Token lifetime. The DB row is re-checked on every request (see
/// [`crate::auth::verify`]), so this only bounds how long a token stays
/// valid after the user record is *deleted* — `disable` takes effect
/// immediately regardless.
const TOKEN_TTL_HOURS: i64 = 12;
const MIN_PASSWORD_LEN: usize = 8;

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

// ---- JWT minting ---------------------------------------------------

/// Mint a signed HS256 token. `None` on a signing failure (logged) —
/// the caller maps that to a 500.
fn mint_jwt(sub: &str, role: Role) -> Option<(String, i64)> {
    let exp = (Utc::now() + Duration::hours(TOKEN_TTL_HOURS)).timestamp();
    let claims = Claims {
        sub: sub.to_string(),
        exp,
        aud: Some(EXPECTED_AUDIENCE.to_string()),
        roles: vec![role.as_str().to_string()],
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
    if disabled != 0 || !verify_password(&req.password, &hash) {
        return Err(unauthorized());
    }
    let Some(role) = Role::parse(&role) else {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "corrupt role"));
    };

    let (token, exp) = mint_jwt(&req.username, role)
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
}

/// `GET /api/auth/me` — the caller's own identity + effective role.
/// Drives the SPA's UI gating (hide operator/admin actions).
pub async fn me(claims: axum::Extension<Claims>) -> Json<MeResp> {
    Json(MeResp {
        username: claims.sub.clone(),
        role: claims.role(),
    })
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
    if !verify_password(&req.old_password, &hash) {
        return Err(err(StatusCode::UNAUTHORIZED, "old password incorrect"));
    }
    let new_hash = hash_password(&req.new_password)
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

#[derive(Serialize, sqlx::FromRow)]
pub struct UserRow {
    username: String,
    role: String,
    disabled: i64,
    must_change_pw: i64,
    created_at: String,
    updated_at: String,
}

/// `GET /api/accounts` — admin. Never returns password hashes.
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<UserRow>>, Response> {
    let rows = sqlx::query_as::<_, UserRow>(
        "SELECT username, role, disabled, must_change_pw, created_at, updated_at FROM users ORDER BY username",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct CreateReq {
    username: String,
    password: String,
    role: String,
}

/// `POST /api/accounts` — admin. Creates a user; `409` on duplicate.
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<CreateReq>,
) -> Result<StatusCode, Response> {
    let username = req.username.trim();
    if username.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "username required"));
    }
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "password too short (min 8 chars)",
        ));
    }
    let Some(role) = Role::parse(&req.role) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "role must be viewer/operator/admin",
        ));
    };
    let hash = hash_password(&req.password)
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "hash failed"))?;

    let res = sqlx::query(
        "INSERT INTO users (username, password_hash, role, must_change_pw) VALUES (?, ?, ?, 0)",
    )
    .bind(username)
    .bind(&hash)
    .bind(role.as_str())
    .execute(&state.pool)
    .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "username already exists"));
        }
        Err(e) => return Err(db_err(e)),
    }

    audit::record(
        &state.nats,
        "admin",
        "account.create",
        Some(username),
        Some(&caller),
        serde_json::json!({ "role": role.as_str() }),
    )
    .await;
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
pub struct UpdateReq {
    role: Option<String>,
    password: Option<String>,
    disabled: Option<bool>,
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
        let hash = hash_password(password)
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
