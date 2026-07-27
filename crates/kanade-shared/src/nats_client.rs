//! Shared NATS client constructor.
//!
//! Every binary names the [`NatsRole`] it connects as, and the token is
//! resolved per role (first match wins):
//!
//!   1. Windows registry — `HKLM\SOFTWARE\kanade\<role>\NatsToken`
//!      (`REG_SZ`). The role-specific credential. Hardened ACL (SYSTEM +
//!      Admin only) keeps the token out of low-privilege users' reach,
//!      which Machine-scope env vars cannot do.
//!   2. Windows registry — `HKLM\SOFTWARE\kanade\agent\NatsToken`. The
//!      **shared** credential every role used before roles existed. Kept as
//!      a fallback so an existing deployment keeps working untouched; see
//!      "Staged migration" below.
//!   3. `$KANADE_NATS_TOKEN` environment variable. Dev / fallback path. The
//!      agent service runs as LocalSystem so user-session env vars never
//!      reach it; this branch only fires for `cargo run` / interactive
//!      shells.
//!   4. No token — connect unauthenticated. Works against a broker started
//!      without `authorization { … }`.
//!
//! # Why roles exist here (#1155)
//!
//! The broker authorises a *connection*, and a connection is only as
//! specific as the credential that opened it. While every binary presented
//! the same token, the broker could not tell an agent from the backend from
//! the CLI, so no `permissions` block could say "only the backend may
//! subscribe `remote.frame.>`" — there was nothing to hang the rule on.
//! That is why a shared token means a token holder can execute code on any
//! endpoint and silently watch any remote-assistance session (#1140).
//!
//! Distinct credentials do not fix that by themselves; the broker config has
//! to grow the matching `authorization { users: [...] }` entries. This
//! module is the half that makes those entries *expressible*.
//!
//! # Staged migration
//!
//! Step 2 above is the whole migration strategy. A fleet running today has
//! one token, provisioned at `…\kanade\agent\NatsToken` on every host
//! regardless of role. After this change it keeps working: no role key
//! exists, so every role falls through to the shared one and presents
//! exactly what it presented before.
//!
//! Rolling out per-role credentials is then per-host and reversible — write
//! `…\kanade\backend\NatsToken` on the backend host and it starts using it;
//! delete it and it falls back. The broker only needs to start
//! *distinguishing* the roles once every host has its own, so the config
//! change lands last, when it can no longer lock anyone out.
//!
//! No deploy script writes a role key yet: `deploy-backend.ps1` still
//! provisions the shared path, so today the role key is a manual registry
//! write. That is deliberate — the scripted path should start writing role
//! keys in the same change that teaches the broker to tell the roles apart,
//! because until then a role key changes nothing and a script that writes
//! only the role key (dropping the shared one) would strand the CLI on a
//! backend-only host.
//!
//! The order matters and is deliberate: role key first, shared key second.
//! The reverse would make the shared token permanent — a host that still has
//! it (all of them, today) would never notice its role key.
//!
//! # Limits worth naming
//!
//! A per-role token still cannot express per-*agent* identity. A role
//! credential permitted to subscribe `commands.pc.*` lets any agent holding
//! it read another agent's inbox. This narrows a fleet-wide compromise to a
//! fleet-wide **agent-role** compromise, which is better, not solved. The
//! end state is per-agent identity (NKeys / NATS-JWT), for which the plan is
//! to grow `ConnectOptions` here so every binary picks up the upgrade for
//! free. Same for mTLS.

use anyhow::{Context, Result};

use crate::secrets;

const ENV_TOKEN: &str = "KANADE_NATS_TOKEN";
const REG_VALUE: &str = "NatsToken";

/// Registry subkey holding the pre-#1155 shared credential. Also the agent's
/// role key, which is not a coincidence — the shared token was provisioned
/// under the agent's path because agents were the first thing to need it.
const REG_SHARED_SUBKEY: &str = r"SOFTWARE\kanade\agent";

/// Which kanade binary is opening the connection.
///
/// Named on every call rather than inferred, because the broker's view of a
/// connection comes entirely from the credential it presents: a caller that
/// picks the wrong role does not get a warning, it gets someone else's
/// permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatsRole {
    /// The endpoint agent. The most numerous and least trusted role — one
    /// compromised endpoint holds this credential.
    Agent,
    /// The backend. The only role that needs to see the whole fleet.
    Backend,
    /// The operator CLI, including the backend-down recovery path that
    /// drives agents over NATS directly.
    Cli,
}

impl NatsRole {
    pub fn as_str(self) -> &'static str {
        match self {
            NatsRole::Agent => "agent",
            NatsRole::Backend => "backend",
            NatsRole::Cli => "cli",
        }
    }

    /// Registry subkey holding this role's credential.
    fn reg_subkey(self) -> String {
        format!(r"SOFTWARE\kanade\{}", self.as_str())
    }
}

/// Resolve a role's token, given a registry reader and the environment
/// fallback.
///
/// Split from [`resolve_token`] so the *ordering* — the part that decides
/// whether a migration is reversible — is testable without a Windows
/// registry to write to.
fn resolve_token_with(
    role: NatsRole,
    read_reg: impl Fn(&str, &str) -> Option<String>,
    env: Option<String>,
) -> Option<String> {
    if let Some(t) = read_reg(&role.reg_subkey(), REG_VALUE) {
        return Some(t);
    }
    if let Some(t) = read_reg(REG_SHARED_SUBKEY, REG_VALUE) {
        return Some(t);
    }
    env.filter(|t| !t.is_empty())
}

fn resolve_token(role: NatsRole) -> Option<String> {
    resolve_token_with(
        role,
        secrets::read_hklm_value,
        std::env::var(ENV_TOKEN).ok(),
    )
}

/// Connect to NATS at `url` as `role`. Resolves the bearer token from the
/// registry (Windows) or `$KANADE_NATS_TOKEN`; connects unauthenticated when
/// neither is set.
pub async fn connect(role: NatsRole, url: &str) -> Result<async_nats::Client> {
    connect_inner(
        role,
        url,
        None::<fn(async_nats::Event) -> std::future::Ready<()>>,
    )
    .await
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
pub async fn connect_with_event_callback<F, Fut>(
    role: NatsRole,
    url: &str,
    cb: F,
) -> Result<async_nats::Client>
where
    F: Fn(async_nats::Event) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + Sync + 'static,
{
    connect_inner(role, url, Some(cb)).await
}

async fn connect_inner<F, Fut>(
    role: NatsRole,
    url: &str,
    cb: Option<F>,
) -> Result<async_nats::Client>
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
    let opts = async_nats::ConnectOptions::new()
        .retry_on_initial_connect()
        // Names the connection in `nats server report connections` and in
        // the broker's own logs. Free observability while the fleet is
        // mid-migration: it shows which roles are connecting even before
        // their credentials differ, which is exactly the window in which a
        // wrongly-provisioned host is otherwise invisible.
        .name(format!("kanade-{}", role.as_str()));
    let opts = match resolve_token(role) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A stand-in registry. Keys are `subkey\value`.
    fn reg(entries: &[(&str, &str)]) -> impl Fn(&str, &str) -> Option<String> {
        let map: HashMap<String, String> = entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |subkey: &str, value: &str| map.get(&format!(r"{subkey}\{value}")).cloned()
    }

    #[test]
    fn role_subkeys_are_distinct_and_agent_matches_the_shared_path() {
        assert_eq!(NatsRole::Backend.reg_subkey(), r"SOFTWARE\kanade\backend");
        assert_eq!(NatsRole::Cli.reg_subkey(), r"SOFTWARE\kanade\cli");
        // The agent's role key IS the historical shared key, so an agent
        // never sees a migration at all.
        assert_eq!(NatsRole::Agent.reg_subkey(), REG_SHARED_SUBKEY);
    }

    #[test]
    fn an_unmigrated_fleet_keeps_presenting_the_shared_token() {
        // The state of every host today: one token, under the agent path.
        let registry = reg(&[(r"SOFTWARE\kanade\agent\NatsToken", "shared")]);
        for role in [NatsRole::Agent, NatsRole::Backend, NatsRole::Cli] {
            assert_eq!(
                resolve_token_with(role, &registry, None).as_deref(),
                Some("shared"),
                "{role:?} must keep working before its own key is provisioned"
            );
        }
    }

    #[test]
    fn a_role_key_wins_over_the_shared_one() {
        let registry = reg(&[
            (r"SOFTWARE\kanade\agent\NatsToken", "shared"),
            (r"SOFTWARE\kanade\backend\NatsToken", "backend-only"),
        ]);
        // The migrated role uses its own credential...
        assert_eq!(
            resolve_token_with(NatsRole::Backend, &registry, None).as_deref(),
            Some("backend-only")
        );
        // ...while a role that has not been migrated yet is unaffected.
        assert_eq!(
            resolve_token_with(NatsRole::Cli, &registry, None).as_deref(),
            Some("shared")
        );
    }

    #[test]
    fn removing_a_role_key_falls_back_rather_than_failing() {
        // Rollback of a per-host migration step: the role key is gone, and
        // the host must return to the shared credential instead of
        // connecting unauthenticated (which a broker with `authorization`
        // would refuse — turning a rollback into an outage).
        let registry = reg(&[(r"SOFTWARE\kanade\agent\NatsToken", "shared")]);
        assert_eq!(
            resolve_token_with(NatsRole::Backend, &registry, None).as_deref(),
            Some("shared")
        );
    }

    #[test]
    fn the_registry_outranks_the_environment() {
        // Unchanged from before roles existed: a dev shell's env var must
        // not quietly override a provisioned production credential.
        let registry = reg(&[(r"SOFTWARE\kanade\agent\NatsToken", "shared")]);
        assert_eq!(
            resolve_token_with(NatsRole::Agent, &registry, Some("from-env".into())).as_deref(),
            Some("shared")
        );
    }

    #[test]
    fn the_environment_serves_hosts_with_no_registry_at_all() {
        let empty = reg(&[]);
        assert_eq!(
            resolve_token_with(NatsRole::Cli, &empty, Some("from-env".into())).as_deref(),
            Some("from-env")
        );
        // An empty env var is not a credential — it must fall through to
        // "no token" so a dev broker without `authorization` still works,
        // rather than presenting the empty string and being rejected.
        assert_eq!(
            resolve_token_with(NatsRole::Cli, &empty, Some(String::new())),
            None
        );
        assert_eq!(resolve_token_with(NatsRole::Cli, &empty, None), None);
    }
}
