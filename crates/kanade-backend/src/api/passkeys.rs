//! Passkey (WebAuthn) HTTP routes. The cryptography and storage live in
//! [`crate::passkey`]; this module is the thin axum layer: feature gating,
//! re-authentication for enrolment/removal, session minting and audit.
//!
//! Interaction with the other credentials (also in `book/src/operations`):
//!
//!   * Passkey sign-in is its own factor-complete path: it does **not** ask
//!     for a TOTP code, because a user-verified passkey is possession plus
//!     PIN/biometric.
//!   * So that this cannot become a way *around* TOTP, enrolling or removing
//!     a passkey needs the current password **and** (when enabled) a current
//!     TOTP code. A stolen session alone can never add a credential.
//!   * Passkeys are additive: the password is never removed, password/TOTP
//!     sign-in and the CLI keep working, and recovery is unchanged (reset
//!     link, plus an admin can clear all of an account's passkeys).
//!   * Disabled accounts, `must_change_pw`, and the login throttle apply to
//!     passkey sign-in exactly as to the password form.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::warn;
use webauthn_rs::prelude::{
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse,
};

use super::AppState;
use super::accounts::{
    LoginResp, client_ip, db_err, err, mint_jwt, session_ttl_hours, too_many, verify_password_async,
};
use crate::audit::{self, Caller};
use crate::auth::Claims;
use crate::http_error::ApiError;
use crate::passkey::{
    ChallengeError, LoginErrorKind, PasskeyInfo, PasskeyService, ReauthProof, RegisterError,
    clean_name,
};

fn service(state: &AppState) -> Result<&PasskeyService, ApiError> {
    state
        .passkeys
        .as_deref()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "passkeys are not enabled"))
}

const BAD_CHALLENGE: &str = "challenge expired or already used; start again";

fn challenge_err(e: ChallengeError) -> ApiError {
    match e {
        ChallengeError::Invalid => err(StatusCode::BAD_REQUEST, BAD_CHALLENGE),
        ChallengeError::Busy => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many sign-ins in progress; try again shortly",
        ),
        ChallengeError::RateLimited(secs) => too_many(secs),
    }
}

// ---- public feature probe ---------------------------------------------

#[derive(Serialize)]
pub struct ConfigResp {
    enabled: bool,
}

/// `GET /api/auth/passkey/config` — public. Lets the SPA decide whether to
/// show any passkey UI at all (the login screen has no session to ask with).
pub async fn config(State(state): State<AppState>) -> Json<ConfigResp> {
    Json(ConfigResp {
        enabled: state.passkeys.is_some(),
    })
}

// ---- sign-in -----------------------------------------------------------

#[derive(Serialize)]
pub struct LoginStartResp {
    challenge_id: String,
    options: RequestChallengeResponse,
}

/// `POST /api/auth/passkey/login/start` — public. Issues a single-use,
/// 2-minute challenge. No username is accepted or revealed.
pub async fn login_start(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LoginStartResp>, ApiError> {
    let svc = service(&state)?;
    let ip = client_ip(&headers);
    let (challenge_id, options) = svc.start_login(&ip).map_err(challenge_err)?;
    Ok(Json(LoginStartResp {
        challenge_id,
        options,
    }))
}

#[derive(Deserialize)]
pub struct LoginFinishReq {
    challenge_id: String,
    credential: PublicKeyCredential,
}

/// `POST /api/auth/passkey/login/finish` — public. On success returns the
/// same body as a password sign-in (including `must_change_pw`).
pub async fn login_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Caller,
    Json(req): Json<LoginFinishReq>,
) -> Result<Json<LoginResp>, ApiError> {
    let svc = service(&state)?;
    let ip = client_ip(&headers);
    let outcome = svc
        .finish_login(
            &state.pool,
            &state.login_throttle,
            &ip,
            &req.challenge_id,
            &req.credential,
        )
        .await;
    let ok = match outcome {
        Ok(ok) => ok,
        Err(f) => {
            audit::record(
                &state.nats,
                "admin",
                "passkey.login_failed",
                f.username.as_deref(),
                Some(&caller),
                serde_json::json!({ "reason": f.reason, "ip": ip }),
            )
            .await;
            return Err(match f.kind {
                LoginErrorKind::Throttled(secs) => too_many(secs),
                LoginErrorKind::Challenge => err(StatusCode::BAD_REQUEST, BAD_CHALLENGE),
                LoginErrorKind::Invalid => err(StatusCode::UNAUTHORIZED, "passkey sign-in failed"),
                LoginErrorKind::Internal => err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "auth backend unavailable",
                ),
            });
        }
    };
    let ttl_hours = session_ttl_hours(&state).await;
    let (token, exp) = mint_jwt(&ok.username, ok.role, ttl_hours)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "token mint failed"))?;
    audit::record(
        &state.nats,
        "admin",
        "passkey.login",
        Some(&ok.username),
        Some(&caller),
        serde_json::json!({ "ip": ip }),
    )
    .await;
    Ok(Json(LoginResp {
        token,
        role: ok.role,
        must_change_pw: ok.must_change_pw,
        exp,
    }))
}

// ---- re-authentication --------------------------------------------------

#[derive(Deserialize)]
pub struct Reauth {
    password: String,
    /// Required (and checked) only when the account has TOTP enabled.
    #[serde(default)]
    totp_code: Option<String>,
}

/// Prove the caller is the account holder *now*: current password and, when
/// TOTP is on, a current code. Failures feed the same lockout as sign-in.
/// Returns the account's `must_change_pw` so callers can refuse enrolment.
async fn reauthenticate(
    state: &AppState,
    headers: &HeaderMap,
    claims: &Claims,
    re: &Reauth,
) -> Result<ReauthProof, ApiError> {
    let username = &claims.sub;
    let ip = client_ip(headers);
    if let Err(secs) = state.login_throttle.check(username, &ip) {
        return Err(too_many(secs));
    }
    let row = sqlx::query_as::<_, (String, i64, i64, Option<String>, String)>(
        "SELECT password_hash, disabled, must_change_pw, totp_secret, passkey_enrollment_epoch FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_err)?;
    // Service tokens / auth-disabled mode have no `users` row, hence nothing
    // to attach a credential to.
    let Some((hash, disabled, must_change_pw, totp_secret, enrollment_epoch)) = row else {
        return Err(err(
            StatusCode::FORBIDDEN,
            "passkeys need a personal user account",
        ));
    };
    // 400, not 401: the SPA treats any 401 as "session expired" and logs the
    // user out, which is wrong for a mistyped password on an authenticated
    // page (the MFA routes make the same choice).
    let fail = || {
        state.login_throttle.record_failure(username, &ip);
        err(
            StatusCode::BAD_REQUEST,
            "current password or authenticator code incorrect",
        )
    };
    if disabled != 0 || !verify_password_async(re.password.clone(), hash).await {
        return Err(fail());
    }
    if let Some(secret) = totp_secret.as_deref().filter(|s| !s.is_empty()) {
        let Some(code) = re
            .totp_code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            return Err(fail());
        };
        match crate::mfa::verify(secret, username, code) {
            Ok(true) => {}
            Ok(false) => return Err(fail()),
            Err(e) => {
                warn!(error = %e, "totp verify failed during passkey re-auth");
                return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "mfa check failed"));
            }
        }
    }
    Ok(ReauthProof {
        must_change_pw: must_change_pw != 0,
        enrollment_epoch,
    })
}

// ---- enrolment ------------------------------------------------------------

#[derive(Deserialize)]
pub struct RegisterStartReq {
    name: String,
    #[serde(flatten)]
    reauth: Reauth,
}

#[derive(Serialize)]
pub struct RegisterStartResp {
    challenge_id: String,
    options: serde_json::Value,
}

/// `POST /api/auth/passkeys/register/start` — self-service. Needs the current
/// password (+ TOTP code when enabled); refused while a forced password change
/// is pending.
pub async fn register_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    claims: axum::Extension<Claims>,
    Json(req): Json<RegisterStartReq>,
) -> Result<Json<RegisterStartResp>, ApiError> {
    let svc = service(&state)?;
    let Some(name) = clean_name(&req.name) else {
        return Err(err(StatusCode::BAD_REQUEST, "name must be 1-64 characters"));
    };
    let proof = reauthenticate(&state, &headers, &claims, &req.reauth).await?;
    if proof.must_change_pw {
        return Err(err(
            StatusCode::FORBIDDEN,
            "change your password before adding a passkey",
        ));
    }
    let (challenge_id, options) = svc
        .start_register(&state.pool, &claims.sub, &name, &proof)
        .await
        .map_err(register_err)?;
    Ok(Json(RegisterStartResp {
        challenge_id,
        options,
    }))
}

#[derive(Deserialize)]
pub struct RegisterFinishReq {
    challenge_id: String,
    credential: RegisterPublicKeyCredential,
}

/// `POST /api/auth/passkeys/register/finish` — self-service. The challenge is
/// bound to the account that started it and to the session's user.
pub async fn register_finish(
    State(state): State<AppState>,
    claims: axum::Extension<Claims>,
    caller: Caller,
    Json(req): Json<RegisterFinishReq>,
) -> Result<(StatusCode, Json<PasskeyInfo>), ApiError> {
    let svc = service(&state)?;
    let info = svc
        .finish_register(&state.pool, &claims.sub, &req.challenge_id, &req.credential)
        .await
        .map_err(register_err)?;
    audit::record(
        &state.nats,
        "admin",
        "passkey.register",
        Some(&claims.sub),
        Some(&caller),
        serde_json::json!({ "passkey_id": info.id, "name": info.name }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(info)))
}

fn register_err(e: RegisterError) -> ApiError {
    match e {
        RegisterError::Challenge(c) => challenge_err(c),
        RegisterError::Rejected => err(
            StatusCode::BAD_REQUEST,
            "the authenticator response was rejected (user verification and the configured origin are required)",
        ),
        RegisterError::Duplicate => err(
            StatusCode::CONFLICT,
            "this authenticator is already registered",
        ),
        RegisterError::LimitReached => err(
            StatusCode::CONFLICT,
            "passkey limit reached; remove one first",
        ),
        RegisterError::Db(e) => db_err(e),
    }
}

// ---- listing / removal -------------------------------------------------------

/// `GET /api/auth/passkeys` — self-service list (no key material).
pub async fn list(
    State(state): State<AppState>,
    claims: axum::Extension<Claims>,
) -> Result<Json<Vec<PasskeyInfo>>, ApiError> {
    service(&state)?;
    Ok(Json(
        crate::passkey::list(&state.pool, &claims.sub)
            .await
            .map_err(db_err)?,
    ))
}

/// `POST /api/auth/passkeys/{id}/remove` — self-service; same re-auth as
/// enrolment. POST rather than DELETE so the credentials in the body survive
/// proxies that drop DELETE bodies.
pub async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    claims: axum::Extension<Claims>,
    caller: Caller,
    Path(id): Path<i64>,
    Json(req): Json<Reauth>,
) -> Result<StatusCode, ApiError> {
    service(&state)?;
    reauthenticate(&state, &headers, &claims, &req).await?;
    if !crate::passkey::remove(&state.pool, &claims.sub, id)
        .await
        .map_err(db_err)?
    {
        return Err(err(StatusCode::NOT_FOUND, "no such passkey"));
    }
    audit::record(
        &state.nats,
        "admin",
        "passkey.remove",
        Some(&claims.sub),
        Some(&caller),
        serde_json::json!({ "passkey_id": id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/accounts/{username}/passkeys` — admin. The device-loss
/// recovery path: revokes pending enrollments and drops every passkey so it falls back to
/// password (+ TOTP). Works even while passkeys are not configured, so an
/// operator who turned the feature off can still purge stale credentials.
pub async fn admin_reset(
    State(state): State<AppState>,
    Path(username): Path<String>,
    caller: Caller,
) -> Result<StatusCode, ApiError> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = ?")
        .bind(&username)
        .fetch_one(&state.pool)
        .await
        .map_err(db_err)?;
    if exists == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    let removed = crate::passkey::remove_all(&state.pool, &username)
        .await
        .map_err(db_err)?;
    audit::record(
        &state.nats,
        "admin",
        "passkey.admin_reset",
        Some(&username),
        Some(&caller),
        serde_json::json!({ "removed": removed }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    //! Handler-level tests: the real router and `auth::verify` over an
    //! in-memory SQLite, a software authenticator, and a NATS client that is
    //! never connected (audit publishes are best-effort and just buffer).

    use super::*;
    use crate::api::accounts::hash_password_async;
    use crate::passkey::tests::{ORIGIN, create, handle_of, pool, sign};
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::SqlitePool;
    use std::time::Duration;
    use tower::ServiceExt;
    use webauthn_authenticator_rs::softpasskey::SoftPasskey;

    const PW: &str = "correct horse battery";

    struct App {
        router: Router,
        pool: SqlitePool,
        state: AppState,
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

    async fn app(passkeys_enabled: bool) -> App {
        let pool = pool().await;
        let hash = hash_password_async(PW.to_string()).await.unwrap();
        sqlx::query("UPDATE users SET password_hash = ?")
            .bind(&hash)
            .execute(&pool)
            .await
            .unwrap();
        let nats = async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .request_timeout(Some(Duration::from_millis(200)))
            .connect("nats://127.0.0.1:9")
            .await
            .expect("an unconnected client is still a client");
        let state = AppState {
            pool: pool.clone(),
            query_pool: pool.clone(),
            commands: std::sync::Arc::new(crate::command_publisher::CommandPublisher::new(
                nats.clone(),
                None,
            )),
            nats: nats.clone(),
            jetstream: async_nats::jetstream::new(nats),
            explode_spec_cache: Default::default(),
            sql_view_cache: crate::api::view_sql::new_cache(),
            group_cache: crate::api::group_sql::new_cache(),
            mailer: None,
            public_url: None,
            nats_url: String::new(),
            login_throttle: Default::default(),
            passkeys: passkeys_enabled.then(|| std::sync::Arc::new(crate::passkey::tests::svc())),
        };
        let router = crate::api::router(state.clone()).layer(axum::middleware::from_fn_with_state(
            pool.clone(),
            crate::auth::verify,
        ));
        App {
            router,
            pool,
            state,
        }
    }

    async fn call(
        app: &App,
        method: &str,
        uri: &str,
        who: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(who) = who {
            req = req.header("authorization", format!("Bearer {}", mint(who)));
        }
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
        let json = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, json)
    }

    async fn start_register(
        app: &App,
        who: &str,
        password: &str,
        totp: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut body = serde_json::json!({ "name": "laptop", "password": password });
        if let Some(t) = totp {
            body["totp_code"] = t.into();
        }
        call(
            app,
            "POST",
            "/api/auth/passkeys/register/start",
            Some(who),
            Some(body),
        )
        .await
    }

    /// Full enrolment through HTTP; returns the new credential's id.
    async fn enroll(app: &App, authn: &mut SoftPasskey, who: &str, totp: Option<&str>) -> i64 {
        let (st, started) = start_register(app, who, PW, totp).await;
        assert_eq!(st, StatusCode::OK, "{started}");
        let reg = create(authn, ORIGIN, started["options"].clone(), |_| {});
        let (st, info) = call(
            app,
            "POST",
            "/api/auth/passkeys/register/finish",
            Some(who),
            Some(serde_json::json!({
                "challenge_id": started["challenge_id"],
                "credential": reg,
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{info}");
        info["id"].as_i64().unwrap()
    }

    async fn passkey_login(
        app: &App,
        authn: &mut SoftPasskey,
        who: &str,
    ) -> (StatusCode, serde_json::Value) {
        let h = handle_of(&app.pool, who).await;
        passkey_login_as(app, authn, &h).await
    }

    /// Sign in with a handle captured earlier (the row may be gone by now).
    async fn passkey_login_as(
        app: &App,
        authn: &mut SoftPasskey,
        h: &(webauthn_rs::prelude::Uuid, Vec<u8>),
    ) -> (StatusCode, serde_json::Value) {
        let (st, started) = call(app, "POST", "/api/auth/passkey/login/start", None, None).await;
        assert_eq!(st, StatusCode::OK, "{started}");
        let cred = sign(authn, ORIGIN, started["options"].clone(), h, |_| {});
        call(
            app,
            "POST",
            "/api/auth/passkey/login/finish",
            None,
            Some(serde_json::json!({
                "challenge_id": started["challenge_id"],
                "credential": cred,
            })),
        )
        .await
    }

    async fn enable_totp(app: &App, user: &str) -> String {
        let secret = crate::mfa::generate_secret_base32();
        sqlx::query("UPDATE users SET totp_secret = ? WHERE username = ?")
            .bind(&secret)
            .bind(user)
            .execute(&app.pool)
            .await
            .unwrap();
        secret
    }

    #[tokio::test]
    async fn feature_is_invisible_when_not_configured() {
        let app = app(false).await;
        let (st, body) = call(&app, "GET", "/api/auth/passkey/config", None, None).await;
        assert_eq!((st, &body["enabled"]), (StatusCode::OK, &false.into()));
        for (m, uri) in [
            ("POST", "/api/auth/passkey/login/start"),
            ("GET", "/api/auth/passkeys"),
        ] {
            let (st, _) = call(&app, m, uri, Some("alice"), None).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{uri}");
        }
        let (st, _) = start_register(&app, "alice", PW, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn public_routes_need_no_token_and_management_routes_do() {
        let app = app(true).await;
        let (st, body) = call(&app, "GET", "/api/auth/passkey/config", None, None).await;
        assert_eq!((st, &body["enabled"]), (StatusCode::OK, &true.into()));
        let (st, _) = call(&app, "POST", "/api/auth/passkey/login/start", None, None).await;
        assert_eq!(st, StatusCode::OK);
        for (m, uri) in [
            ("GET", "/api/auth/passkeys"),
            ("POST", "/api/auth/passkeys/register/start"),
            ("POST", "/api/auth/passkeys/register/finish"),
            ("POST", "/api/auth/passkeys/1/remove"),
            ("DELETE", "/api/accounts/alice/passkeys"),
        ] {
            let (st, _) = call(&app, m, uri, None, Some(serde_json::json!({}))).await;
            assert_eq!(st, StatusCode::UNAUTHORIZED, "{m} {uri}");
        }
    }

    #[tokio::test]
    async fn enrol_login_list_and_remove_round_trip() {
        let app = app(true).await;
        let mut authn = SoftPasskey::new(true);
        let id = enroll(&app, &mut authn, "alice", None).await;
        let h = handle_of(&app.pool, "alice").await;

        let (st, list) = call(&app, "GET", "/api/auth/passkeys", Some("alice"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "laptop");
        assert!(
            list[0].get("passkey").is_none(),
            "no key material leaves the server"
        );

        let (st, sess) = passkey_login(&app, &mut authn, "alice").await;
        assert_eq!(st, StatusCode::OK, "{sess}");
        assert_eq!(sess["role"], "operator");
        assert_eq!(sess["must_change_pw"], false);
        // The minted token is an ordinary session: the DB-backed middleware accepts it.
        let token = sess["token"].as_str().unwrap();
        let (st, _) = {
            let resp = app
                .router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/auth/me")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            (resp.status(), ())
        };
        assert_eq!(st, StatusCode::OK);

        // Removal re-authenticates, and only removes the caller's own.
        let (st, _) = call(
            &app,
            "POST",
            &format!("/api/auth/passkeys/{id}/remove"),
            Some("alice"),
            Some(serde_json::json!({ "password": "wrong" })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(
            &app,
            "POST",
            &format!("/api/auth/passkeys/{id}/remove"),
            Some("bob"),
            Some(serde_json::json!({ "password": PW })),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "bob cannot remove alice's passkey"
        );
        let (st, _) = call(
            &app,
            "POST",
            &format!("/api/auth/passkeys/{id}/remove"),
            Some("alice"),
            Some(serde_json::json!({ "password": PW })),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, _) = passkey_login_as(&app, &mut authn, &h).await;
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "a removed passkey no longer signs in"
        );
    }

    #[tokio::test]
    async fn enrolment_needs_the_current_password() {
        let app = app(true).await;
        let (st, _) = start_register(&app, "alice", "nope", None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = start_register(&app, "alice", PW, None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn totp_accounts_must_present_a_code_to_add_or_remove_passkeys() {
        let app = app(true).await;
        let secret = enable_totp(&app, "alice").await;

        // Password alone — i.e. a hijacked session — is not enough.
        let (st, _) = start_register(&app, "alice", PW, None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = start_register(&app, "alice", PW, Some("000000")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        let code = crate::mfa::current_code(&secret, "alice");
        let mut authn = SoftPasskey::new(true);
        let id = enroll(&app, &mut authn, "alice", Some(&code)).await;

        // Removal asks for the same proof.
        let uri = format!("/api/auth/passkeys/{id}/remove");
        let (st, _) = call(
            &app,
            "POST",
            &uri,
            Some("alice"),
            Some(serde_json::json!({ "password": PW })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(
            &app,
            "POST",
            &uri,
            Some("alice"),
            Some(serde_json::json!({ "password": PW, "totp_code": code })),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn passkey_login_is_its_own_factor_but_password_login_still_demands_totp() {
        let app = app(true).await;
        let secret = enable_totp(&app, "alice").await;
        let mut authn = SoftPasskey::new(true);
        let code = crate::mfa::current_code(&secret, "alice");
        enroll(&app, &mut authn, "alice", Some(&code)).await;

        // Passkey sign-in (UV) completes without a TOTP prompt…
        let (st, sess) = passkey_login(&app, &mut authn, "alice").await;
        assert_eq!(st, StatusCode::OK, "{sess}");
        assert!(sess["token"].is_string());

        // …but the existing password path is untouched: password alone still
        // gets `mfa_required`, never a session.
        let (st, body) = call(
            &app,
            "POST",
            "/api/auth/login",
            None,
            Some(serde_json::json!({ "username": "alice", "password": PW })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["mfa_required"], true);
        assert!(body.get("token").is_none());
        // And a wrong password is still a plain 401.
        let (st, _) = call(
            &app,
            "POST",
            "/api/auth/login",
            None,
            Some(serde_json::json!({ "username": "alice", "password": "bad" })),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn forced_password_change_blocks_enrolment_and_survives_passkey_login() {
        let app = app(true).await;
        let mut authn = SoftPasskey::new(true);
        enroll(&app, &mut authn, "alice", None).await;
        sqlx::query("UPDATE users SET must_change_pw = 1 WHERE username = 'alice'")
            .execute(&app.pool)
            .await
            .unwrap();
        // Cannot add credentials until the password is changed…
        let (st, _) = start_register(&app, "alice", PW, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // …and an existing passkey does not clear the gate.
        let (st, sess) = passkey_login(&app, &mut authn, "alice").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(sess["must_change_pw"], true);
    }

    #[tokio::test]
    async fn disabled_accounts_are_refused() {
        let app = app(true).await;
        let mut authn = SoftPasskey::new(true);
        enroll(&app, &mut authn, "alice", None).await;
        sqlx::query("UPDATE users SET disabled = 1 WHERE username = 'alice'")
            .execute(&app.pool)
            .await
            .unwrap();
        let (st, body) = passkey_login(&app, &mut authn, "alice").await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert!(body.get("token").is_none());
        // Same opaque message as any other failure.
        assert_eq!(body, "passkey sign-in failed");
    }

    #[tokio::test]
    async fn page_restricted_accounts_can_still_manage_their_own_passkeys() {
        let app = app(true).await;
        sqlx::query("UPDATE users SET allowed_features = '[]' WHERE username = 'alice'")
            .execute(&app.pool)
            .await
            .unwrap();
        let mut authn = SoftPasskey::new(true);
        enroll(&app, &mut authn, "alice", None).await;
        let (st, _) = call(&app, "GET", "/api/auth/passkeys", Some("alice"), None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_reset_is_admin_only_and_restores_password_only_access() {
        let app = app(true).await;
        let mut authn = SoftPasskey::new(true);
        enroll(&app, &mut authn, "bob", None).await;
        let h = handle_of(&app.pool, "bob").await;
        let (st, pending) = start_register(&app, "bob", PW, None).await;
        assert_eq!(st, StatusCode::OK);
        let cred = create(
            &mut SoftPasskey::new(true),
            ORIGIN,
            pending["options"].clone(),
            |_| {},
        );

        let (st, _) = call(
            &app,
            "DELETE",
            "/api/accounts/bob/passkeys",
            Some("alice"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "operators cannot reset");
        let (st, _) = call(
            &app,
            "DELETE",
            "/api/accounts/bob/passkeys",
            Some("root"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (st, _) = call(
            &app,
            "POST",
            "/api/auth/passkeys/register/finish",
            Some("bob"),
            Some(serde_json::json!({
                "challenge_id": pending["challenge_id"], "credential": cred,
            })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(
            crate::passkey::list(&app.pool, "bob")
                .await
                .unwrap()
                .is_empty()
        );

        let (st, _) = call(
            &app,
            "DELETE",
            "/api/accounts/nobody/passkeys",
            Some("root"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = passkey_login_as(&app, &mut authn, &h).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        // The recovery path is the ordinary one: the password still works.
        let (st, body) = call(
            &app,
            "POST",
            "/api/auth/login",
            None,
            Some(serde_json::json!({ "username": "bob", "password": PW })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(body["token"].is_string());
    }

    #[tokio::test]
    async fn deleting_an_account_removes_its_passkeys_so_a_recreated_name_inherits_nothing() {
        let app = app(true).await;
        let mut authn = SoftPasskey::new(true);
        enroll(&app, &mut authn, "alice", None).await;
        let old_token = mint("alice");
        let (st, pending) = start_register(&app, "alice", PW, None).await;
        assert_eq!(st, StatusCode::OK);
        let cred = create(&mut authn, ORIGIN, pending["options"].clone(), |_| {});

        let count = |app: &App| {
            let pool = app.pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_passkeys")
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count(&app).await, 1);
        let (st, _) = call(&app, "DELETE", "/api/accounts/alice", Some("root"), None).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert_eq!(count(&app).await, 0, "credentials die with the account");

        // Even a leftover row (e.g. restored from an old snapshot) is purged
        // when the name is created afresh.
        sqlx::query(
            "INSERT INTO user_passkeys (username, user_uuid, credential_id, passkey, name) \
             VALUES ('alice', 'u', 'stale', '{}', 'stale')",
        )
        .execute(&app.pool)
        .await
        .unwrap();
        let (st, _) = call(
            &app,
            "POST",
            "/api/accounts",
            Some("root"),
            Some(serde_json::json!({
                "username": "alice", "password": PW, "role": "viewer"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(count(&app).await, 0);
        // A JWT and challenge issued to the previous holder of the name
        // must not enroll a credential into the recreated account.
        let resp = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/passkeys/register/finish")
                    .header("authorization", format!("Bearer {old_token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "challenge_id": pending["challenge_id"], "credential": cred,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(count(&app).await, 0);
        // The new account can still enroll through a fresh password proof.
        enroll(&app, &mut SoftPasskey::new(true), "alice", None).await;
        assert_eq!(count(&app).await, 1);
    }

    #[tokio::test]
    async fn last_admin_delete_keeps_the_admins_passkeys() {
        let app = app(true).await;
        let mut authn = SoftPasskey::new(true);
        enroll(&app, &mut authn, "root", None).await;
        let (st, _) = call(&app, "DELETE", "/api/accounts/root", Some("root"), None).await;
        assert_eq!(st, StatusCode::CONFLICT);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_passkeys")
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "a refused delete must not strip credentials");
    }
    #[tokio::test]
    async fn enrollment_cannot_survive_invalidation_of_reauthentication_snapshot() {
        let app = app(true).await;
        let secret = enable_totp(&app, "alice").await;
        let claims = Claims {
            sub: "alice".into(),
            exp: 4_102_444_800,
            aud: Some(crate::auth::EXPECTED_AUDIENCE.into()),
            roles: vec![],
            allowed_features: None,
        };
        let reauth = Reauth {
            password: PW.into(),
            totp_code: Some(crate::mfa::current_code(&secret, "alice")),
        };
        // Exercise the actual password/TOTP verifier, including the epoch
        // read in the SAME row snapshot as those factors.
        let proof = reauthenticate(&app.state, &HeaderMap::new(), &claims, &reauth)
            .await
            .unwrap_or_else(|_| panic!("reauthentication failed"));
        let (status, _) = call(
            &app,
            "DELETE",
            "/api/accounts/alice/passkeys",
            Some("root"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let current: String = sqlx::query_scalar(
            "SELECT passkey_enrollment_epoch FROM users WHERE username = 'alice'",
        )
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_ne!(proof.enrollment_epoch, current);
        let svc = service(&app.state).unwrap_or_else(|_| panic!("passkeys disabled"));
        // The reset precedes start, which was the gap in the previous fix:
        // re-reading here would silently bless the invalidated proof.
        let (id, options) = svc
            .start_register(&app.pool, "alice", "stale proof", &proof)
            .await
            .unwrap();
        let credential = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
        let result = svc
            .finish_register(&app.pool, "alice", &id, &credential)
            .await;
        assert!(matches!(result, Err(RegisterError::Rejected)), "{result:?}");
        assert!(
            crate::passkey::list(&app.pool, "alice")
                .await
                .unwrap()
                .is_empty()
        );
        // Rejection is due to the generation, not a broken attestation path.
        let fresh = reauthenticate(&app.state, &HeaderMap::new(), &claims, &reauth)
            .await
            .unwrap_or_else(|_| panic!("fresh reauthentication failed"));
        let (id, options) = svc
            .start_register(&app.pool, "alice", "fresh proof", &fresh)
            .await
            .unwrap();
        let credential = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
        svc.finish_register(&app.pool, "alice", &id, &credential)
            .await
            .unwrap();
        assert_eq!(
            crate::passkey::list(&app.pool, "alice")
                .await
                .unwrap()
                .len(),
            1
        );
        // The same rule holds when a username is deleted and recreated with
        // different factors, rather than just reset in place.
        let (status, _) = call(&app, "DELETE", "/api/accounts/alice", Some("root"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(
            &app,
            "POST",
            "/api/accounts",
            Some("root"),
            Some(serde_json::json!({
                "username": "alice", "password": "different account password", "role": "viewer"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        enable_totp(&app, "alice").await;
        assert!(
            reauthenticate(&app.state, &HeaderMap::new(), &claims, &reauth)
                .await
                .is_err()
        );
        let (id, options) = svc
            .start_register(&app.pool, "alice", "previous holder", &fresh)
            .await
            .unwrap();
        let credential = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
        assert!(matches!(
            svc.finish_register(&app.pool, "alice", &id, &credential)
                .await,
            Err(RegisterError::Rejected)
        ));
        assert!(
            crate::passkey::list(&app.pool, "alice")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
