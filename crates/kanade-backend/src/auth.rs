//! Authentication middleware for `/api/*`. Three modes, picked at
//! request time:
//!
//!   1. `KANADE_AUTH_DISABLE=1` — open access. Dev / local-only.
//!   2. Static-token mode — shared bearer. Caller sends
//!      `Authorization: Bearer <secret>`; middleware does a
//!      constant-time compare. Simplest production-ish mode and
//!      enough for single-operator fleets.
//!   3. JWT mode (HS256, Sprint 4c) — sign tokens out-of-band with
//!      `aud=kanade` + an `exp`.
//!
//! Each secret is resolved registry-first, env-second:
//!
//! ```text
//! StaticToken:  HKLM\SOFTWARE\kanade\backend\StaticToken
//!               → $KANADE_AUTH_STATIC_TOKEN
//! JwtSecret:    HKLM\SOFTWARE\kanade\backend\JwtSecret
//!               → $KANADE_JWT_SECRET
//! ```
//!
//! Registry values are written by `deploy-backend.ps1 -StaticToken …
//! -JwtSecret …` with an ACL hardened to SYSTEM + Administrators
//! only — keeping the secret out of low-privilege users' reach, which
//! Machine-scope env vars cannot do. The env vars stay around for
//! dev / cargo-make / non-Windows hosts. `KANADE_AUTH_DISABLE` stays
//! env-only since it's a presence flag, not a secret.
//!
//! Precedence: DISABLE > STATIC_TOKEN > JWT_SECRET. The first one
//! that resolves wins; the others are ignored. With none set the
//! middleware falls back to JWT mode with a loud-warning hard-coded
//! dev secret — convenient when you forget to set anything, awful
//! in production.
//!
//! The real OIDC story (JWKS fetch, RS256, issuer validation, role
//! scopes per claim) is still on the backlog. Sprint 7's static
//! token mode covers the gap so `KANADE_AUTH_DISABLE` isn't the
//! only way to talk to /api/*.
//!
//! Example JWT signing:
//!
//! ```rust,no_run
//! use jsonwebtoken::{encode, EncodingKey, Header, Algorithm};
//! use serde_json::json;
//! let token = encode(
//!     &Header::new(Algorithm::HS256),
//!     &json!({ "sub": "alice", "aud": "kanade", "exp": 2_000_000_000_i64, "roles": ["operator"] }),
//!     &EncodingKey::from_secret(b"dev-secret"),
//! ).unwrap();
//! ```

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use kanade_shared::secrets;
use serde::{Deserialize, Serialize};
use std::env;
use tracing::warn;

/// JWT claims yielded by the validator. Stashed in the request
/// extensions so downstream handlers can introspect the caller.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Claims {
    pub sub: String,
    pub exp: i64,
    #[serde(default)]
    pub aud: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

const ENV_DISABLE: &str = "KANADE_AUTH_DISABLE";
const ENV_STATIC_TOKEN: &str = "KANADE_AUTH_STATIC_TOKEN";
const ENV_SECRET: &str = "KANADE_JWT_SECRET";
const REG_SUBKEY: &str = r"SOFTWARE\kanade\backend";
const REG_STATIC_TOKEN: &str = "StaticToken";
const REG_JWT_SECRET: &str = "JwtSecret";
const EXPECTED_AUDIENCE: &str = "kanade";

fn resolve_static_token() -> Option<String> {
    if let Some(t) = secrets::read_hklm_value(REG_SUBKEY, REG_STATIC_TOKEN) {
        return Some(t);
    }
    match env::var(ENV_STATIC_TOKEN) {
        Ok(t) if !t.is_empty() => Some(t),
        _ => None,
    }
}

fn resolve_jwt_secret() -> Option<String> {
    if let Some(s) = secrets::read_hklm_value(REG_SUBKEY, REG_JWT_SECRET) {
        return Some(s);
    }
    match env::var(ENV_SECRET) {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

pub async fn verify(req: Request, next: Next) -> Result<Response, Response> {
    // Auth opt-out for local development. Set KANADE_AUTH_DISABLE=1 to
    // run the whole stack without tokens.
    if env::var(ENV_DISABLE).is_ok() {
        return Ok(next.run(req).await);
    }

    // Only /api/* is protected; the SPA static files at / and the
    // health probe at /health stay public.
    let path = req.uri().path();
    if !path.starts_with("/api/") {
        return Ok(next.run(req).await);
    }

    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        return Err(unauth("missing bearer token"));
    };

    // Static-token mode: simple shared bearer secret.
    if let Some(expected) = resolve_static_token() {
        return if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
            // Synthesise a Claims so downstream handlers that look in
            // request extensions for a caller identity get a usable
            // value (sub=static-token, no roles, far-future exp).
            let mut req = req;
            req.extensions_mut().insert(Claims {
                sub: "static-token".to_string(),
                exp: 4_102_444_800, // 2100-01-01
                aud: Some(EXPECTED_AUDIENCE.to_string()),
                roles: Vec::new(),
            });
            Ok(next.run(req).await)
        } else {
            warn!(path, "static-token mismatch");
            Err(unauth("invalid static token"))
        };
    }

    let secret = resolve_jwt_secret().unwrap_or_else(|| {
        warn!(
            "no StaticToken/JwtSecret registry value and no KANADE_AUTH_STATIC_TOKEN/KANADE_JWT_SECRET env var — using a hard-coded dev fallback (NEVER in production)"
        );
        "dev-secret-please-override".to_string()
    });

    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&[EXPECTED_AUDIENCE]);

    match decode::<Claims>(token, &key, &validation) {
        Ok(data) => {
            let mut req = req;
            req.extensions_mut().insert(data.claims);
            Ok(next.run(req).await)
        }
        Err(e) => {
            warn!(error = %e, path, "JWT verify failed");
            Err(unauth(&format!("invalid token: {e}")))
        }
    }
}

/// Length-checked, branch-free byte comparison. Tiny inline impl —
/// we don't want a crate dep just for this and the use site is a
/// non-adversarial config path anyway, but the constant-time shape
/// future-proofs us if the token surface ever gets exposed to
/// untrusted callers.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn unauth(msg: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Body::from(msg.to_owned())).into_response()
}
