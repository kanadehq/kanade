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
//! # What the broker will and will not accept (measured, #1270)
//!
//! Two nats-server behaviours constrain every plan built on this module, so
//! they are recorded here rather than rediscovered:
//!
//! * A config may not carry **both** a `token` and a `users` array —
//!   nats-server refuses to start: *"Can not have a token and a users
//!   array"*. And once `users` are defined, a client presenting a token is
//!   rejected with an Authorization Violation, even when the token equals a
//!   user's password. So the shared token and a per-role `users` split
//!   cannot coexist for a transition window: the flip is atomic, and every
//!   host must already hold a credential of the new shape before it happens.
//!   Resolving a *token* per role, which is all this module does today, is
//!   therefore not sufficient for that split — the client has to learn to
//!   present a user as well.
//! * `/connz?auth=1` reports `authorized_user` per connection. Under
//!   `users` that is the username — the per-host answer #1270 wants. Under
//!   `token`, nats-server 2.14.3 reports the literal `[REDACTED]`: it hides
//!   the credential, so token mode can say *that* a host is on the shared
//!   token but never anything finer. Whether the value is hidden is the
//!   broker build's choice, not ours, so a consumer must assume it may be
//!   handling a secret; see [`CredentialProbe`] for the one question it can
//!   safely ask about one.
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

/// Prefix every kanade connection announces in its `name`.
const NAME_PREFIX: &str = "kanade-";

/// Separator between the role and the host identity in a connection name.
/// `/` is safe as a delimiter because the identity is a Windows computer
/// name, which cannot contain one.
const NAME_SEP: char = '/';

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

/// The `name` a kanade process announces on its NATS connection.
///
/// Without an identity this is `kanade-<role>`; with one it is
/// `kanade-<role>/<identity>`. The broker echoes it back verbatim in
/// `/connz`, which is what lets the backend attribute a connection — and
/// therefore the credential the broker authenticated it with — to a pc_id
/// (#1270). Nothing else on a connection carries the pc_id: the CID is
/// assigned by the server and the IP is not a stable identifier on a fleet
/// of laptops.
///
/// The name is client-supplied and therefore claimed, not proved. What
/// `/connz` makes unforgeable is the *credential* half of the pair; a host
/// can still lie about which pc_id it is. Under one fleet-wide token that
/// changes nothing (every host can already impersonate every other), and
/// closing it for good is per-agent identity, not a naming convention.
pub fn client_name(role: NatsRole, identity: Option<&str>) -> String {
    match identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => format!("{NAME_PREFIX}{}{NAME_SEP}{id}", role.as_str()),
        None => format!("{NAME_PREFIX}{}", role.as_str()),
    }
}

/// A connection name split back into its parts — see [`client_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientName<'a> {
    /// The role segment as announced. A `&str` rather than a [`NatsRole`]
    /// on purpose: a connection from a future (or foreign) build may name a
    /// role this binary does not know, and dropping it on the floor would
    /// hide exactly the host worth looking at.
    pub role: &'a str,
    /// The host identity, when the connection carried one. `None` for the
    /// backend / CLI (which are not per-host) and for agents predating
    /// #1270 — those simply cannot be attributed.
    pub identity: Option<&'a str>,
}

/// Parse a connection name produced by [`client_name`]. `None` for any name
/// that is not a kanade connection at all (a `nats` CLI session, a
/// monitoring tool), which the caller should ignore rather than guess about.
pub fn parse_client_name(name: &str) -> Option<ClientName<'_>> {
    let rest = name.strip_prefix(NAME_PREFIX)?;
    Some(match rest.split_once(NAME_SEP) {
        // An empty identity (`kanade-agent/`) is not an identity.
        Some((role, id)) if !role.is_empty() && !id.is_empty() => ClientName {
            role,
            identity: Some(id),
        },
        Some((role, _)) if !role.is_empty() => ClientName {
            role,
            identity: None,
        },
        Some(_) => return None,
        None if !rest.is_empty() => ClientName {
            role: rest,
            identity: None,
        },
        None => return None,
    })
}

/// Answers "is this credential the one *we* present?" without handing the
/// credential itself to the caller.
///
/// #1270: the NATS monitoring endpoint reports `authorized_user` per
/// connection. A current nats-server hides that field for
/// token-authenticated connections, but that is the broker build's
/// behaviour, not a guarantee this side can lean on — a consumer of
/// `/connz` has to treat the value as possibly being the fleet-wide secret.
/// The one question it may safely answer about it is whether it equals the
/// credential this process already holds, and that answer is enough to
/// label a connection ("still on the shared token") without ever storing or
/// serving the value.
///
/// Constructed once and reused: [`resolve_token`] hits the Windows registry,
/// and the caller compares against every connection on the broker.
pub struct CredentialProbe {
    presented: Credential,
}

/// What a process presents when it connects.
enum Credential {
    /// Nothing — the dev path, against a broker with no `authorization`.
    None,
    /// A bearer token. The only shape [`connect`] can present today.
    Token(String),
    /// A named user. Reserved for the client half of #1266; see
    /// [`CredentialKind::User`] for why the distinction matters here.
    User { name: String },
}

/// Which shape of credential a [`CredentialProbe`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    None,
    Token,
    /// A named user — and, when the connection presenting it is **live**,
    /// positive proof that the broker is running `users` rather than a
    /// token: nats-server refuses to load a config carrying both a `token`
    /// and a `users` array ("Can not have a token and a users array") and
    /// rejects token authentication outright once `users` are defined.
    ///
    /// That proof is the only thing that makes a reported `authorized_user`
    /// safe to record verbatim. Note what is *not* proof: holding no
    /// credential locally. A process that never authenticated at all can
    /// still read a monitoring endpoint, and inferring the broker's mode
    /// from a local absence would let a misconfigured host store the very
    /// secret the rest of this type exists to protect.
    User,
}

/// Hand-written so a stray `{:?}` in a log line cannot print the credential.
/// A username is not a secret and is shown; a token never is.
impl std::fmt::Debug for CredentialProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rendered = match &self.presented {
            Credential::None => "<none>".to_string(),
            Credential::Token(_) => "<redacted token>".to_string(),
            Credential::User { name } => format!("user {name}"),
        };
        f.debug_struct("CredentialProbe")
            .field("presented", &rendered)
            .finish()
    }
}

impl CredentialProbe {
    /// Resolve the credential `role` would present, exactly as [`connect`]
    /// does.
    pub fn for_role(role: NatsRole) -> Self {
        Self {
            presented: match resolve_token(role) {
                Some(t) => Credential::Token(t),
                None => Credential::None,
            },
        }
    }

    /// Build a probe around an explicitly-supplied token.
    ///
    /// The production path is [`Self::for_role`]; this exists so callers can
    /// be tested against a known credential without a Windows registry to
    /// write to — and, on a developer's machine, without accidentally
    /// probing the real fleet token that `for_role` would find there.
    pub fn from_token(token: Option<String>) -> Self {
        Self {
            presented: match token {
                Some(t) => Credential::Token(t),
                None => Credential::None,
            },
        }
    }

    /// Build a probe for a process that authenticates as a named user.
    ///
    /// Nothing constructs this in production yet — [`connect`] cannot
    /// present a user (#1266). It exists so the consumers of
    /// [`CredentialKind::User`] are testable now rather than written blind
    /// later.
    pub fn from_user(name: impl Into<String>) -> Self {
        Self {
            presented: Credential::User { name: name.into() },
        }
    }

    /// Which shape of credential this process presents.
    pub fn kind(&self) -> CredentialKind {
        match &self.presented {
            Credential::None => CredentialKind::None,
            Credential::Token(_) => CredentialKind::Token,
            Credential::User { .. } => CredentialKind::User,
        }
    }

    /// Whether `candidate` is the **secret** this process presents.
    ///
    /// Only ever true for a token. A user's secret is its password, which
    /// `authorized_user` never carries — matching a username here would
    /// mean "this connection is on the same account", a different and much
    /// weaker statement than the one callers use this for.
    ///
    /// A plain comparison: `candidate` comes from the broker's own report of
    /// connections it already authenticated, not from an attacker-chosen
    /// input, so there is no oracle to time.
    pub fn is_ours(&self, candidate: &str) -> bool {
        match &self.presented {
            Credential::Token(t) => t == candidate,
            Credential::None | Credential::User { .. } => false,
        }
    }
}

/// Connect to NATS at `url` as `role`. Resolves the bearer token from the
/// registry (Windows) or `$KANADE_NATS_TOKEN`; connects unauthenticated when
/// neither is set.
///
/// The connection is announced as `kanade-<role>` with no host identity —
/// right for the backend and the CLI, which are not per-host. A role that
/// has to be attributable to a specific machine (the agent) must use
/// [`connect_with_event_callback`] and pass one; see [`client_name`].
pub async fn connect(role: NatsRole, url: &str) -> Result<async_nats::Client> {
    connect_inner(
        role,
        url,
        None,
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
///
/// `identity` names the host this connection belongs to (the agent's
/// pc_id). It becomes part of the connection name the broker echoes in
/// `/connz`, which is the only thing tying a connection — and the
/// credential that opened it — back to a machine (#1270).
pub async fn connect_with_event_callback<F, Fut>(
    role: NatsRole,
    url: &str,
    identity: Option<&str>,
    cb: F,
) -> Result<async_nats::Client>
where
    F: Fn(async_nats::Event) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + Sync + 'static,
{
    connect_inner(role, url, identity, Some(cb)).await
}

async fn connect_inner<F, Fut>(
    role: NatsRole,
    url: &str,
    identity: Option<&str>,
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
        // Names the connection in `nats server report connections`, in the
        // broker's own logs, and in `/connz`. Free observability while the
        // fleet is mid-migration: it shows which roles are connecting even
        // before their credentials differ, which is exactly the window in
        // which a wrongly-provisioned host is otherwise invisible. With an
        // identity it also carries the pc_id, so #1270 can join the broker's
        // per-connection `authorized_user` back onto the agents row.
        .name(client_name(role, identity));
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

    // ── #1270: connection naming ─────────────────────────────────────

    #[test]
    fn an_identity_round_trips_through_the_connection_name() {
        // The pc_id is the join key between `/connz` and the agents table,
        // so the name has to survive the trip unchanged — including the
        // casing, which is NOT uniform across the fleet and which NATS
        // subjects treat as significant.
        for pc in ["PC001", "minipc", "Web%01", "ws-9"] {
            let name = client_name(NatsRole::Agent, Some(pc));
            let parsed = parse_client_name(&name).expect("our own name must parse");
            assert_eq!(parsed.role, "agent");
            assert_eq!(parsed.identity, Some(pc));
        }
    }

    #[test]
    fn a_role_without_an_identity_keeps_the_pre_1270_name() {
        // The backend and the CLI are not per-host, and an agent that
        // predates #1270 announces this shape too. Both must parse as "a
        // kanade connection we cannot attribute" rather than as an error or
        // as an empty pc_id.
        assert_eq!(client_name(NatsRole::Backend, None), "kanade-backend");
        let parsed = parse_client_name("kanade-agent").unwrap();
        assert_eq!(parsed.role, "agent");
        assert_eq!(parsed.identity, None);
        // Whitespace-only is not an identity either — it would otherwise
        // produce a name that parses back into a pc_id no row can match.
        assert_eq!(client_name(NatsRole::Agent, Some("  ")), "kanade-agent");
    }

    #[test]
    fn foreign_connections_do_not_parse_as_kanade_ones() {
        // A `nats` CLI session or a monitoring tool shares the broker. The
        // projector must skip those rather than attribute them to a host.
        assert!(parse_client_name("NATS CLI Version 0.1.5").is_none());
        assert!(parse_client_name("").is_none());
        assert!(parse_client_name("kanade-").is_none());
        assert!(parse_client_name("kanade-/PC001").is_none());
        // A trailing separator with no identity is a role, not a pc_id.
        assert_eq!(parse_client_name("kanade-agent/").unwrap().identity, None);
    }

    #[test]
    fn an_unknown_role_is_preserved_rather_than_dropped() {
        // A future build (or something impersonating one) naming a role this
        // binary has never heard of is precisely the connection an operator
        // wants to see.
        let parsed = parse_client_name("kanade-relay/PC001").unwrap();
        assert_eq!(parsed.role, "relay");
        assert_eq!(parsed.identity, Some("PC001"));
    }

    // ── #1270: credential probe ──────────────────────────────────────

    #[test]
    fn the_probe_recognises_only_the_credential_we_present() {
        let probe = CredentialProbe::from_token(Some("shared".into()));
        assert_eq!(probe.kind(), CredentialKind::Token);
        assert!(probe.is_ours("shared"));
        assert!(!probe.is_ours("something-else"));
        // The empty string is what the broker reports for a connection it
        // did not authenticate at all. It must never read as "ours".
        assert!(!probe.is_ours(""));
    }

    #[test]
    fn a_probe_with_no_credential_matches_nothing() {
        // Dev broker with no `authorization` block. We hold nothing, so we
        // can prove nothing about anyone else's credential — including that
        // it is safe to store.
        let probe = CredentialProbe::from_token(None);
        assert_eq!(probe.kind(), CredentialKind::None);
        assert!(!probe.is_ours(""));
        assert!(!probe.is_ours("anything"));
    }

    #[test]
    fn a_username_is_not_a_secret_we_can_recognise() {
        // `is_ours` answers "is this MY secret", and a user's secret is its
        // password. Matching on the username instead would answer a much
        // weaker question while reading like the strong one.
        let probe = CredentialProbe::from_user("kanade-backend");
        assert_eq!(probe.kind(), CredentialKind::User);
        assert!(!probe.is_ours("kanade-backend"));
    }

    #[test]
    fn the_probe_never_prints_the_credential() {
        // `/connz` handling logs liberally; one `{:?}` on the probe must not
        // be the thing that puts the fleet's token in a log file.
        let probe = CredentialProbe::from_token(Some("super-secret-token".into()));
        let rendered = format!("{probe:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(
            format!("{:?}", CredentialProbe::from_token(None)).contains("none"),
            "the no-credential case should be visible, just not the value"
        );
        // A username is not a secret — hiding it would cost diagnosability
        // for nothing.
        assert!(
            format!("{:?}", CredentialProbe::from_user("kanade-backend"))
                .contains("kanade-backend"),
        );
    }
}
