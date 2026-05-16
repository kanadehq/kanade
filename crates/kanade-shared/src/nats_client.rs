//! Shared NATS client constructor.
//!
//! Token resolution (first match wins):
//!
//!   1. Windows registry — `HKLM\SOFTWARE\kanade\agent\NatsToken`
//!      (`REG_SZ`). Production path. Hardened ACL (SYSTEM + Admin
//!      only) keeps the token out of low-privilege users' reach,
//!      which Machine-scope env vars cannot do.
//!   2. `$KANADE_NATS_TOKEN` environment variable. Dev / fallback
//!      path. The agent service runs as LocalSystem so user-session
//!      env vars never reach it; this branch only fires for
//!      `cargo run` / interactive shells.
//!   3. No token — connect unauthenticated. Works against a broker
//!      started without `authorization { … }`.
//!
//! For mTLS / NKeys / NATS-JWT modes (spec §2.7.1's full design), the
//! plan is to grow ConnectOptions here — every binary picks up the
//! upgrade for free.

use anyhow::{Context, Result};

const ENV_TOKEN: &str = "KANADE_NATS_TOKEN";

#[cfg(windows)]
const REG_PATH: &str = r"SOFTWARE\kanade\agent";
#[cfg(windows)]
const REG_VALUE: &str = "NatsToken";

#[cfg(windows)]
fn read_registry_token() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(REG_PATH).ok()?;
    let token: String = key.get_value(REG_VALUE).ok()?;
    if token.is_empty() { None } else { Some(token) }
}

#[cfg(not(windows))]
fn read_registry_token() -> Option<String> {
    None
}

fn resolve_token() -> Option<String> {
    if let Some(t) = read_registry_token() {
        return Some(t);
    }
    match std::env::var(ENV_TOKEN) {
        Ok(t) if !t.is_empty() => Some(t),
        _ => None,
    }
}

/// Connect to NATS at `url`. Resolves the bearer token from registry
/// (Windows) or `$KANADE_NATS_TOKEN`; connects unauthenticated when
/// neither is set.
pub async fn connect(url: &str) -> Result<async_nats::Client> {
    let opts = async_nats::ConnectOptions::new();
    let opts = match resolve_token() {
        Some(token) => opts.token(token),
        None => opts,
    };
    opts.connect(url)
        .await
        .with_context(|| format!("connect to NATS at {url}"))
}
