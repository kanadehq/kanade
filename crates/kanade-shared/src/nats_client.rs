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

use crate::secrets;

const ENV_TOKEN: &str = "KANADE_NATS_TOKEN";
const REG_SUBKEY: &str = r"SOFTWARE\kanade\agent";
const REG_VALUE: &str = "NatsToken";

fn resolve_token() -> Option<String> {
    if let Some(t) = secrets::read_hklm_value(REG_SUBKEY, REG_VALUE) {
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
    connect_inner(url, None::<fn(async_nats::Event) -> std::future::Ready<()>>).await
}

/// Same as [`connect`] but also wires an `event_callback` that fires
/// whenever async-nats publishes a `ConnectEvent` (Connected,
/// Disconnected, ServerError, etc.). The callback's `Future` runs on
/// the async-nats internal task — keep it cheap and non-blocking
/// (set a flag, send on a channel, that kind of thing) so the
/// connection state machine isn't held up.
///
/// Used by the agent's v0.26 Layer 2 staleness tracker: the callback
/// stamps a shared `Mutex<Option<Instant>>` on every Connected event,
/// so `decide()` at fire time can answer "how long ago were we last
/// definitely-talking-to-the-broker" without a polling loop.
pub async fn connect_with_event_callback<F, Fut>(url: &str, cb: F) -> Result<async_nats::Client>
where
    F: Fn(async_nats::Event) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + Sync + 'static,
{
    connect_inner(url, Some(cb)).await
}

async fn connect_inner<F, Fut>(url: &str, cb: Option<F>) -> Result<async_nats::Client>
where
    F: Fn(async_nats::Event) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + Sync + 'static,
{
    // v0.38 / #137: offline-tolerant boot. Without
    // `retry_on_initial_connect`, `opts.connect(url).await` blocks-then-
    // errors when the broker is unreachable at startup — the agent
    // process dies, SCM ticks its restart counter, and the offline-
    // tolerant subsystems (local_scheduler, outbox drain) never spawn.
    // With this flag, connect() returns `Ok(Client)` immediately and
    // async-nats does the reconnect in the background; subscribe()
    // calls queue the SUB frame until the link is up.
    let opts = async_nats::ConnectOptions::new().retry_on_initial_connect();
    let opts = match resolve_token() {
        Some(token) => opts.token(token),
        None => opts,
    };
    let opts = match cb {
        Some(cb) => opts.event_callback(cb),
        None => opts,
    };
    opts.connect(url)
        .await
        .with_context(|| format!("connect to NATS at {url}"))
}
