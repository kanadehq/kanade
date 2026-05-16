//! Authentication middleware for `/api/*`. Three modes, picked at
//! request time from env:
//!
//!   1. `KANADE_AUTH_DISABLE=1` — open access. Dev / local-only.
//!   2. `KANADE_AUTH_STATIC_TOKEN=<secret>` — shared bearer token.
//!      Caller sends `Authorization: Bearer <secret>`; middleware
//!      does a constant-time compare. Simplest production-ish mode
//!      and enough for single-operator fleets.
//!   3. `KANADE_JWT_SECRET=<secret>` — HS256 JWT mode (Sprint 4c).
//!      Sign tokens out-of-band with `aud=kanade` + a future `exp`.
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
const EXPECTED_AUDIENCE: &str = "kanade";

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

    // Static-token mode: simple shared bearer secret. constant_time_eq
    // would be ideal, but matching the env var is operator-controlled
    // and not in a JWT-style adversarial path — a plain equality
    // compare is acceptable here.
    if let Ok(expected) = env::var(ENV_STATIC_TOKEN) {
        return if !expected.is_empty() && constant_time_eq(token.as_bytes(), expected.as_bytes()) {
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

    let secret = env::var(ENV_SECRET).unwrap_or_else(|_| {
        warn!(
            env = ENV_SECRET,
            "neither KANADE_AUTH_STATIC_TOKEN nor KANADE_JWT_SECRET is set — using a hard-coded dev fallback (NEVER in production)"
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
