//! reqwest::Client factory that auto-attaches the operator's bearer
//! token from `$env:KANADE_AUTH_TOKEN` to every HTTP request.
//!
//! Backend's auth modes (KANADE_AUTH_DISABLE / KANADE_AUTH_STATIC_TOKEN
//! / KANADE_JWT_SECRET) all accept `Authorization: Bearer <token>`
//! when set, so the CLI doesn't need to know which mode the server
//! is running — it just always sends the header when the operator
//! exported a token. Missing env var = no header, which is fine
//! against a KANADE_AUTH_DISABLE=1 dev backend.

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

const ENV_TOKEN: &str = "KANADE_AUTH_TOKEN";
const SOURCE_HEADER: HeaderName = HeaderName::from_static("x-kanade-source");
const SOURCE_VALUE: HeaderValue = HeaderValue::from_static("cli");

/// Build a `reqwest::Client` with `Authorization: Bearer <token>`
/// pre-applied as a default header when `$KANADE_AUTH_TOKEN` is set.
///
/// Every request also carries `X-Kanade-Source: cli`, which the backend
/// folds into operator-initiated audit events as `source: "cli"` so the
/// audit log distinguishes CLI traffic from the SPA's `source: "spa"`.
pub fn authed_client() -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(SOURCE_HEADER, SOURCE_VALUE);
    if let Ok(token) = std::env::var(ENV_TOKEN)
        && !token.is_empty()
    {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("KANADE_AUTH_TOKEN contains non-ASCII / control characters")?;
        headers.insert(AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .context("build reqwest::Client")
}
