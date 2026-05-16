//! Shared NATS client constructor.
//!
//! All three binaries (kanade-agent / kanade-backend / kanade CLI)
//! connect to the same broker, so they share the same auth surface:
//!
//!   * If `$KANADE_NATS_TOKEN` is set, attach it as the bearer token
//!     (NATS-side `authorization.token` mode). The token doesn't end
//!     up in `nats_url` strings or process-listings.
//!   * Otherwise connect unauthenticated, which only works against a
//!     broker that was started without `authorization { … }`.
//!
//! For mTLS / NKeys / NATS-JWT modes (spec §2.7.1's full design), the
//! plan is to grow ConnectOptions here — every binary picks up the
//! upgrade for free.

use anyhow::{Context, Result};

const ENV_TOKEN: &str = "KANADE_NATS_TOKEN";

/// Connect to NATS at `url`. Reads `$KANADE_NATS_TOKEN` and attaches
/// it as a bearer token if set; otherwise connects unauthenticated.
pub async fn connect(url: &str) -> Result<async_nats::Client> {
    let opts = async_nats::ConnectOptions::new();
    let opts = match std::env::var(ENV_TOKEN) {
        Ok(token) if !token.is_empty() => opts.token(token),
        _ => opts,
    };
    opts.connect(url)
        .await
        .with_context(|| format!("connect to NATS at {url}"))
}
