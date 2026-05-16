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
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

const ENV_TOKEN: &str = "KANADE_AUTH_TOKEN";

/// Build a `reqwest::Client` with `Authorization: Bearer <token>`
/// pre-applied as a default header when `$KANADE_AUTH_TOKEN` is set.
pub fn authed_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Ok(token) = std::env::var(ENV_TOKEN)
        && !token.is_empty()
    {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("KANADE_AUTH_TOKEN contains non-ASCII / control characters")?;
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }
    builder.build().context("build reqwest::Client")
}
