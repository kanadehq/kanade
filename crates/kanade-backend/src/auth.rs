//! HS256 JWT middleware for `/api/*`. The real OIDC integration
//! (JWKS fetch, RS256 verification, issuer validation, audience scopes
//! per role) is deferred to Sprint 5; Sprint 4c lands the layer
//! ergonomics so every later endpoint inherits authentication.
//!
//! Disable for development by setting `KANADE_AUTH_DISABLE=1`. Sign tokens
//! out-of-band against `KANADE_JWT_SECRET` (a shared HS256 secret) with
//! `aud=kanade` and a future `exp`. Example via the `jsonwebtoken` crate:
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

    let secret = env::var(ENV_SECRET).unwrap_or_else(|_| {
        warn!(
            env = ENV_SECRET,
            "KANADE_JWT_SECRET unset — using a hard-coded dev fallback (NEVER in production)"
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

fn unauth(msg: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Body::from(msg.to_owned())).into_response()
}
