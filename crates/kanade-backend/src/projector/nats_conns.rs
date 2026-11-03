//! #1270: broker connections → `agents.nats_user`.
//!
//! Every other projector in this directory consumes something the fleet
//! published. This one polls the **broker's** HTTP monitoring endpoint
//! (`/connz`) and projects what NATS says about each live connection: which
//! credential it authenticated with.
//!
//! # Why the broker and not the agent
//!
//! A heartbeat field would be cheaper and wrong twice over. The agent knows
//! which token it was *given*; the broker knows what it was *authenticated
//! as*, and those differ precisely in the case worth catching — a host
//! kitted from a stale image. And heartbeats are unsigned and forgeable
//! (#1269), so the endpoint under audit would be its own witness. NATS
//! cannot be talked out of what it authenticated.
//!
//! This is the credential half of what `command_keys` (#1195) did for the
//! signing rollout: make distribution **enumerable** before anything is
//! tightened. The tightening step of #1266 waits on it.
//!
//! # What the endpoint actually reports
//!
//! `/connz?auth=1` carries `authorized_user` per connection. Measured
//! against nats-server 2.14.3 (see the live test at the bottom of this
//! file), not inferred from the docs:
//!
//! * `users` mode — the **username**, e.g. `kanade-agent`. The answer this
//!   whole projection exists to record.
//! * `token` mode, the fleet's shape today — the literal
//!   [`REDACTED_BY_BROKER`]. The broker hides the credential itself, which
//!   also means it says nothing beyond "this connection authenticated with
//!   the token". With a single global token — the only shape nats-server
//!   accepts alongside no `users` array — that IS the shared token, and so
//!   the fleet lands in one countable bucket, [`LABEL_SHARED_TOKEN`].
//!
//! An empty/missing user also describes a socket awaiting CONNECT. Only
//! broker policy or application activity may establish unauthenticated
//! access; see `Acceptance`. Pending/unproven are health uncertainty, not
//! credentials and not proof of an open broker. `/varz` brackets each walk
//! so restarts/reloads cannot join evidence from different broker epochs.
//!
//! # Why the stored value is still not the reported value
//!
//! That redaction is a courtesy of the broker's build, not an invariant
//! this code controls: a different or older nats-server may report the
//! credential verbatim. Copying the fleet's NATS token into SQLite and
//! serving it from `GET /api/agents` to every operator session would be a
//! worse outcome than the observability gap this closes, so the projector
//! stores only values it can *prove* are not secrets:
//!
//! * the redaction marker, and the token the backend itself presents —
//!   both recorded as [`LABEL_SHARED_TOKEN`], never as their value;
//! * a username, but only against [`Evidence::UsersMode`] — a **live**
//!   backend connection that authenticated as a user. nats-server refuses
//!   to load a config carrying both a token and a users array, and rejects
//!   token auth outright once users exist, so a connection that got in with
//!   a user proves the broker cannot be handing out tokens;
//! * otherwise [`LABEL_UNKNOWN`], which says "connected, credential
//!   unnameable" without disclosing anything.
//!
//! Note what is deliberately *not* treated as evidence: the backend having
//! no local credential. This projector polls plain HTTP and does not stop
//! when the backend's own NATS connection is down, so "we hold no token"
//! can equally mean "this host is misconfigured" — and inferring the
//! broker's mode from that absence would let exactly the wrong host store
//! the fleet's secret verbatim (review #1273, claude).
//!
//! # Correlation, and what it does not prove
//!
//! The join key is the connection name the agent announces
//! (`kanade-agent/<pc_id>`, [`kanade_shared::nats_client::client_name`]).
//! That half is *claimed* by the host, so a host can lie about which pc_id
//! it is — but not about the credential it presented. Under one fleet-wide
//! token that asymmetry costs nothing (anyone holding it can already
//! impersonate anyone); closing it for good is per-agent identity, not a
//! naming convention.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use kanade_shared::nats_client::{CredentialKind, CredentialProbe, NatsRole, parse_client_name};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

use super::nats_auth_audit::{self, PollStatus};
use super::nats_auth_findings::{Claimed, Observed};

/// How often the broker is polled. Slower than the 30 s heartbeat on
/// purpose: a host's credential changes when it is re-kitted or
/// re-provisioned, not minute to minute, and each poll pulls one row per
/// live connection.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Connections requested per `/connz` page. The endpoint's own default cap
/// is 1024; a multi-thousand-host fleet is paged through in order.
const PAGE_SIZE: usize = 1024;

/// Backstop on pages walked per poll, so a broker that keeps reporting a
/// non-decreasing `total` can't spin this loop forever.
const MAX_PAGES: usize = 64;

/// Deadline for one whole poll — connect, every page, and every body.
///
/// `hyper_util`'s legacy client applies no request timeout of its own, so a
/// monitoring port that completes the TCP handshake and then never answers
/// would park `fetch_all` forever. That does not just lose one poll: the
/// loop below awaits it, so the loop stops ticking, and it stops silently —
/// `healthy` stays `Some(true)`, so the "endpoint unreadable" warning never
/// fires either (review #1273, coderabbit).
///
/// Bounding the whole poll rather than each request is deliberate: it also
/// covers the paged walk, which can issue up to [`MAX_PAGES`] requests that
/// individually make progress. Comfortably under [`POLL_INTERVAL`] so a slow
/// poll can never overlap the next tick, and far above what a local
/// monitoring endpoint needs.
const POLL_TIMEOUT: Duration = Duration::from_secs(20);

/// Ceiling on a single `/connz` response body. A page of 1024 connections
/// is ~400 KB; 16 MiB is far above that and far below "the backend OOMs
/// because something else is listening on the monitoring port".
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// What nats-server puts in `authorized_user` for a token-authenticated
/// connection instead of the token (measured on 2.14.3). Not a value we
/// control — see the module docs for why the classifier does not rely on
/// the broker being this careful.
const REDACTED_BY_BROKER: &str = "[REDACTED]";

/// Authenticated with the token — which, on a broker that accepts token
/// auth at all, is the single fleet-wide credential. The migration queue.
pub const LABEL_SHARED_TOKEN: &str = "shared-token";
/// Positive broker evidence of access without an authenticated identity.
pub const LABEL_NO_AUTH: &str = "no-auth";
/// Connected, but the credential cannot be named without disclosing a
/// secret. Distinct from NULL, which means "not seen at all".
pub const LABEL_UNKNOWN: &str = "unknown";
/// No acceptance evidence yet, within the broker's own TCP auth timeout.
pub const LABEL_PENDING: &str = "auth-pending";
/// Authentication acceptance cannot be established from this observation.
pub const LABEL_UNPROVEN: &str = "auth-unproven";
/// Broker-produced v1 proof of authentication rejection. Never a username.
pub const LABEL_REJECTED: &str = "auth-rejected";
/// Acceptance is proven, but a safe public credential name is unavailable.
pub const LABEL_ACCEPTED_UNNAMED: &str = "auth-accepted-unnameable";

/// Fixed, secret-free classification vocabulary; never contains broker values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceReason {
    BrokerOpenConfig,
    AcceptedTrafficNoAuth,
    PreConnectPending,
    IncompleteMonitoring,
    ListenerAuthUnproven,
    InvalidConnectionStart,
    AuthPolicyConflict,
    AcceptanceUnproven,
    AuthenticationRejected,
}

#[derive(Debug, Clone, Copy)]
enum Acceptance {
    NoAuth(EvidenceReason),
    Pending,
    Unproven(EvidenceReason),
}

impl Default for Acceptance {
    fn default() -> Self {
        Self::Unproven(EvidenceReason::IncompleteMonitoring)
    }
}

impl Acceptance {
    fn label(self) -> &'static str {
        match self {
            Self::NoAuth(_) => LABEL_NO_AUTH,
            Self::Pending => LABEL_PENDING,
            Self::Unproven(_) => LABEL_UNPROVEN,
        }
    }

    fn reason(self) -> EvidenceReason {
        match self {
            Self::NoAuth(reason) | Self::Unproven(reason) => reason,
            Self::Pending => EvidenceReason::PreConnectPending,
        }
    }
}

/// Aggregate only identity-less, unproven observations, without changing the
/// companion evidence ledger's `Observed` shape.
fn uncertainty_reasons(conns: &[ConnInfo]) -> BTreeMap<EvidenceReason, usize> {
    let mut counts = BTreeMap::new();
    for c in conns {
        if c.identity_label.as_deref() == Some(LABEL_REJECTED) {
            *counts
                .entry(EvidenceReason::AuthenticationRejected)
                .or_default() += 1;
            continue;
        }
        if Claimed::from_name(c.name.as_deref()).is_some()
            && c.authorized_user
                .as_deref()
                .is_none_or(|u| u.trim().is_empty())
            && matches!(c.acceptance, Acceptance::Pending | Acceptance::Unproven(_))
        {
            *counts.entry(c.acceptance.reason()).or_default() += 1;
        }
    }
    counts
}

/// Versioned broker result; never Debug-print any unvalidated broker value.
#[derive(Deserialize)]
struct AuthIdentity {
    version: u32,
    server_id: String,
    server_start: DateTime<Utc>,
    cid: u64,
    connection_start: DateTime<Utc>,
    transport: String,
    generation: u64,
    sequence: u64,
    state: String,
    mechanism: String,
    source: String,
    public_user: Option<String>,
}

impl std::fmt::Debug for AuthIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthIdentity(<unvalidated>)")
    }
}

fn public_identity(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !matches!(
            name,
            LABEL_UNKNOWN
                | LABEL_SHARED_TOKEN
                | LABEL_NO_AUTH
                | LABEL_PENDING
                | LABEL_UNPROVEN
                | LABEL_REJECTED
                | LABEL_ACCEPTED_UNNAMED
        )
}

/// The administrative monitoring endpoint is the authority. v1 is emitted
/// only by the reviewed producer patch, after the broker checks credentials;
/// CONNECT names, authorized_user and expected-mode settings never enter it.
fn verified_identity(c: &ConnInfo, broker: &Varz, observed: DateTime<Utc>) -> Option<String> {
    let a = match &c.auth_identity {
        Some(a) => a,
        None if broker.auth_identity_version.is_none() => return None,
        None => return Some(LABEL_UNKNOWN.into()),
    };
    let valid = a.version == 1
        && broker.auth_identity_version == Some(1)
        && ((a.state == "pending" && a.generation == 0)
            || (a.generation != 0
                && Some(a.generation) == broker.auth_identity_generation
                && a.sequence != 0))
        && a.server_id == broker.server_id
        && a.server_start == broker.start
        && a.cid != 0
        && a.cid == c.cid
        && Some(a.connection_start) == c.start
        && c.kind.as_deref() == Some("Client")
        && c.transport.as_deref() == Some(a.transport.as_str())
        && matches!(a.transport.as_str(), "nats" | "websocket")
        && a.connection_start >= broker.start
        && a.connection_start <= broker.now
        && (observed - broker.now).num_seconds().abs() <= 60;
    Some(if !valid {
        LABEL_UNKNOWN.into()
    } else {
        match (a.state.as_str(), a.mechanism.as_str()) {
            ("accepted", "user_password") if matches!(a.source.as_str(), "connect" | "cookie") => a
                .public_user
                .as_deref()
                .filter(|n| public_identity(n))
                .unwrap_or(if a.public_user.is_none() {
                    LABEL_ACCEPTED_UNNAMED
                } else {
                    LABEL_UNKNOWN
                })
                .into(),
            ("accepted", "token") => LABEL_SHARED_TOKEN.into(),
            (
                "accepted",
                "nkey" | "jwt" | "tls_mapping" | "delegated" | "custom" | "unavailable",
            ) => LABEL_ACCEPTED_UNNAMED.into(),
            // An assigned no_auth_user is not proof of presenting credentials.
            ("accepted", "anonymous" | "no_auth_user") => LABEL_NO_AUTH.into(),
            ("pending" | "validated", _) => LABEL_PENDING.into(),
            ("rejected", _) => LABEL_REJECTED.into(),
            _ => LABEL_UNKNOWN.into(),
        }
    })
}

/// The subset of `/connz` this projector reads.
#[derive(Debug, Deserialize)]
struct Connz {
    /// Connections matching the query across ALL pages — the paging bound.
    total: usize,
    connections: Vec<ConnInfo>,
    #[serde(default)]
    server_id: String,
    now: Option<DateTime<Utc>>,
    offset: Option<usize>,
    num_connections: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ConnInfo {
    #[serde(default)]
    auth_identity: Option<AuthIdentity>,
    #[serde(skip)]
    identity_label: Option<String>,
    #[serde(skip)]
    connection: Option<super::nats_auth_evidence::Connection>,
    /// Broker-issued id, scoped to its startup epoch. Also breaks ties
    /// between live connections claiming one pc_id: higher = more recent.
    #[serde(default)]
    cid: u64,
    #[serde(default)]
    start: Option<chrono::DateTime<Utc>>,
    #[serde(default)]
    last_activity: Option<chrono::DateTime<Utc>>,
    /// The client-supplied connection name. Absent for clients that set
    /// none (the `nats` CLI does set one; a raw library user may not).
    #[serde(default)]
    name: Option<String>,
    /// Present only with `auth=1`. Empty / absent also BEFORE authentication.
    #[serde(default)]
    authorized_user: Option<String>,
    kind: Option<String>,
    #[serde(rename = "type")]
    transport: Option<String>,
    in_msgs: Option<u64>,
    out_msgs: Option<u64>,
    subscriptions: Option<u64>,
    #[serde(skip)]
    acceptance: Acceptance,
}

/// Required fields deliberately have no defaults: `{}` is not a healthy broker.
/// Optional auth_required remains unknown when omitted (NATS uses omitempty).
#[derive(Debug, Deserialize)]
struct Varz {
    auth_identity_version: Option<u32>,
    auth_identity_generation: Option<u64>,
    server_id: String,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    config_load_time: DateTime<Utc>,
    auth_timeout: f64,
    auth_required: Option<bool>,
}

impl Varz {
    fn same_epoch(&self, other: &Self) -> bool {
        !self.server_id.is_empty()
            && self.server_id == other.server_id
            && self.start == other.start
            && self.config_load_time == other.config_load_time
            && self.auth_required == other.auth_required
            && self.auth_timeout == other.auth_timeout
            && self.auth_identity_version == other.auth_identity_version
            && self.auth_identity_generation == other.auth_identity_generation
            && self.now <= other.now
    }

    fn acceptance(&self, c: &ConnInfo, at: DateTime<Utc>) -> Acceptance {
        if c.kind.as_deref() != Some("Client")
            || !matches!(c.transport.as_deref(), Some("nats" | "websocket"))
        {
            return Acceptance::Unproven(EvidenceReason::ListenerAuthUnproven);
        }
        let Some(start) = c.start.filter(|start| *start >= self.start && *start <= at) else {
            return Acceptance::Unproven(EvidenceReason::InvalidConnectionStart);
        };
        // NATS parser gates SUB/PUB behind authentication. These counters
        // count application messages/subscriptions, not INFO, CONNECT or PING.
        // Require all fields: omitted counters are NOT zero.
        let (Some(input), Some(output), Some(subs)) = (c.in_msgs, c.out_msgs, c.subscriptions)
        else {
            return Acceptance::Unproven(EvidenceReason::IncompleteMonitoring);
        };
        // /varz does not expose effective WS authorization (including its
        // token/user overrides and cookies). A second CONNECT overwrites the
        // identity before authentication but retains earlier traffic counters.
        // Without listener policy evidence those counters cannot prove that
        // the currently missing WS identity was accepted.
        if c.transport.as_deref() == Some("websocket") {
            return Acceptance::Unproven(EvidenceReason::ListenerAuthUnproven);
        }
        if input > 0 || output > 0 || subs > 0 {
            // On an auth-required TCP listener, a missing identity conflicts
            // with broker policy. Counters can predate a second CONNECT
            // (which overwrites options before re-authentication), so they
            // cannot prove that this missing identity was accepted.
            if self.auth_required == Some(true) {
                return Acceptance::Unproven(EvidenceReason::AuthPolicyConflict);
            }
            return Acceptance::NoAuth(EvidenceReason::AcceptedTrafficNoAuth);
        }
        if c.transport.as_deref() == Some("nats") {
            if self.auth_required == Some(false) {
                return Acceptance::NoAuth(EvidenceReason::BrokerOpenConfig);
            }
            // A deadline is explanatory only; expiry is never acceptance.
            if self.auth_required == Some(true)
                && self.auth_timeout.is_finite()
                && self.auth_timeout > 0.0
                && (at - start).num_milliseconds() as f64 / 1000.0 < self.auth_timeout
            {
                return Acceptance::Pending;
            }
        }
        // TCP auth flags/timeouts cannot describe WS overrides, cookies or
        // no_auth_user. Idle WS connections stay unproven, not healthy.
        Acceptance::Unproven(if c.transport.as_deref() == Some("websocket") {
            EvidenceReason::ListenerAuthUnproven
        } else {
            EvidenceReason::AcceptanceUnproven
        })
    }
}

fn classify_connection(c: &ConnInfo, probe: &CredentialProbe, ev: Evidence) -> String {
    if let Some(label) = &c.identity_label {
        return label.clone();
    }
    if c.authorized_user
        .as_deref()
        .is_none_or(|u| u.trim().is_empty())
    {
        return c.acceptance.label().to_string();
    }
    // A WS listener may use a different token even while TCP uses users.
    // Never extend TCP's proof that a value is a username to another listener.
    // Nor does a WS users-mode config/username allowlist establish the
    // authenticated user: getRawAuthUser prefers the client-supplied nkey
    // option even when the broker validated a different username/password.
    // The live WSS test exercises that with successful authentication.
    let ev = if c.transport.as_deref() != Some("nats") {
        Evidence::Unproven
    } else {
        ev
    };
    classify(c.authorized_user.as_deref(), probe, ev)
}

/// What this poll can *prove* about the broker's authentication mode.
///
/// Derived from the backend's own live connection, not from configuration:
/// a resolved credential says what this host would present, while only a
/// connection the broker accepted says what the broker accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    /// The backend authenticated as a named user, so the broker is running
    /// `users` — the only state in which a reported `authorized_user` is
    /// known to be a username rather than a secret.
    UsersMode,
    /// The backend authenticated with a token, so the broker is running
    /// `token` and every value it reports is that credential.
    TokenMode,
    /// Nothing is proven: the backend's connection is not up, or it holds
    /// no credential at all. Treat every reported value as possibly secret.
    Unproven,
}

fn evidence(probe: &CredentialProbe, connected: bool) -> Evidence {
    if !connected {
        return Evidence::Unproven;
    }
    match probe.kind() {
        CredentialKind::User => Evidence::UsersMode,
        CredentialKind::Token => Evidence::TokenMode,
        // Connected while presenting nothing means the broker required
        // nothing of *us*. That says nothing about how it authenticated
        // anyone else, so it proves nothing here.
        CredentialKind::None => Evidence::Unproven,
    }
}

/// A local connection proves a credential mode only for its current monitored
/// broker and TCP listener. Reconnecting to a different server, an old cid, or
/// a WS auth override must not make another listener's token safe to store.
fn monitor_evidence(
    probe: &CredentialProbe,
    connected: bool,
    server_id: &str,
    client_id: u64,
    broker: &Varz,
    conns: &[ConnInfo],
) -> Evidence {
    evidence(
        probe,
        connected
            && server_id == broker.server_id
            && broker.auth_required == Some(true)
            && conns.iter().any(|c| {
                c.cid == client_id
                    && c.transport.as_deref() == Some("nats")
                    && c.authorized_user
                        .as_deref()
                        .is_some_and(|u| !u.trim().is_empty())
            }),
    )
}

/// Classify one connection's reported `authorized_user` into a value that
/// is safe to store. See the module docs for why this is not the identity
/// function.
fn classify(authorized_user: Option<&str>, probe: &CredentialProbe, ev: Evidence) -> String {
    match authorized_user.map(str::trim).filter(|s| !s.is_empty()) {
        None => LABEL_UNPROVEN.to_string(),
        // The broker hid the credential, which it only does for token auth.
        // A config that accepts a token has exactly one, so this is the
        // shared credential even though we never see its value.
        Some(u) if u == REDACTED_BY_BROKER => LABEL_SHARED_TOKEN.to_string(),
        // Same conclusion the hard way, for a broker that reports the
        // credential instead of hiding it.
        Some(u) if probe.is_ours(u) => LABEL_SHARED_TOKEN.to_string(),
        // The one path to a stored value: positive proof that the broker
        // deals in usernames, from a live connection of our own that it
        // accepted on one.
        Some(u) if ev == Evidence::UsersMode => u.to_string(),
        // Everything else is a value we cannot vouch for — a token under
        // TokenMode, or anything at all when nothing is proven. Naming it
        // would be the one mistake this module exists to prevent.
        Some(_) => LABEL_UNKNOWN.to_string(),
    }
}

/// Connections the correlation could not use, summarised for the log.
///
/// Both conditions are usually **permanent** — a host kitted with the wrong
/// role name keeps reconnecting under it — so reporting them per connection
/// per poll would emit the same warnings every minute forever (review
/// #1273, coderabbit). Collected here instead, and logged by [`run`] only
/// when the set changes, which keeps the signal and drops the repetition.
#[derive(Debug, Default, PartialEq, Eq)]
struct Anomalies {
    /// `kanade-<something-else>/<pc_id>` — names a host under a role that is
    /// not the agent, so it cannot be attributed to an agents row.
    non_agent: Vec<String>,
    /// Two live connections claiming one pc_id with different credentials.
    conflicting: Vec<String>,
}

impl Anomalies {
    fn is_empty(&self) -> bool {
        self.non_agent.is_empty() && self.conflicting.is_empty()
    }
}

/// One poll's worth of correlated results: pc_id → label, plus whatever
/// could not be correlated.
///
/// Split out from the I/O so the correlation rules — which connections
/// count, and what happens when two claim the same host — are testable
/// without a broker.
fn correlate(
    conns: &[ConnInfo],
    probe: &CredentialProbe,
    ev: Evidence,
) -> (HashMap<String, String>, Anomalies) {
    // pc_id → (cid, label); the highest cid wins a tie.
    let mut best: HashMap<String, (u64, String)> = HashMap::new();
    let mut anomalies = Anomalies::default();
    for c in conns {
        let Some(name) = c.name.as_deref() else {
            continue;
        };
        let Some(parsed) = parse_client_name(name) else {
            continue;
        };
        // Only agents are per-host. A backend / CLI connection carries no
        // identity, and an agent that predates #1270 carries none either —
        // both stay uncorrelated rather than being guessed at.
        let Some(pc_id) = parsed.identity else {
            continue;
        };
        if parsed.role != NatsRole::Agent.as_str() {
            // Something claiming to be a per-host connection in another
            // role. Not attributable to the agents row, and worth saying so
            // out loud rather than silently folding into it.
            anomalies.non_agent.push(name.to_string());
            continue;
        }
        let label = classify_connection(c, probe, ev);
        if matches!(label.as_str(), LABEL_PENDING | LABEL_UNPROVEN) {
            continue;
        }
        match best.get(pc_id) {
            Some((cid, existing)) if *cid >= c.cid => {
                if existing != &label {
                    anomalies
                        .conflicting
                        .push(format!("{pc_id} (kept {existing}, ignored {label})"));
                }
            }
            _ => {
                best.insert(pc_id.to_string(), (c.cid, label));
            }
        }
    }
    // Sorted so "did this change since the last poll" is a comparison of
    // the anomalies themselves, not of the order the broker listed them in.
    anomalies.non_agent.sort();
    anomalies.conflicting.sort();
    let labels = best
        .into_iter()
        .map(|(pc, (_cid, label))| (pc, label))
        .collect();
    (labels, anomalies)
}

/// Every kanade-named (or unnamed) connection with the label the broker's
/// report reduced to, for the authentication audit.
///
/// Unlike [`correlate`] this is not limited to agents and does not collapse
/// two connections claiming one pc_id: an open broker or a wrong role user is
/// visible on a backend, a CLI or a nameless client just as much, and each
/// live connection is its own piece of evidence. The label comes from
/// [`classify`], so the raw `authorized_user` never travels further.
fn observe(conns: &[ConnInfo], probe: &CredentialProbe, ev: Evidence) -> Vec<Observed> {
    conns
        .iter()
        .filter_map(|c| {
            let claimed = Claimed::from_name(c.name.as_deref())?;
            let pc_id = c
                .name
                .as_deref()
                .and_then(parse_client_name)
                .and_then(|p| p.identity)
                .map(str::to_string);
            Some(Observed {
                connection: c.connection.clone(),
                claimed,
                pc_id,
                label: classify_connection(c, probe, ev),
            })
        })
        .collect()
}

/// Write the correlated labels, touching only rows whose label actually
/// changed. Returns how many rows moved.
///
/// Diff first, then write. The `WHERE` guard on the UPDATE already makes an
/// unchanged poll a semantic no-op, but a no-op UPDATE still takes SQLite's
/// single writer lock for the length of the transaction — on a 3,000-host
/// fleet that is 3,000 statements a minute, forever, competing with the
/// projectors that #488 already had to be batched for (review #1273,
/// coderabbit). Reading first costs one indexed scan of a one-row-per-PC
/// table and, in the steady state this projector is in almost always, opens
/// no write transaction at all.
///
/// `nats_user_since` therefore means "since when has it been this", which is
/// the question a migration actually asks. How recently the host was seen at
/// all is `last_heartbeat`.
///
/// Rows for hosts with no live connection are left alone: the last observed
/// credential is still the best answer about an offline machine, exactly as
/// `command_keys` stays put when an agent goes quiet.
async fn apply(pool: &SqlitePool, labels: &HashMap<String, String>) -> Result<u64> {
    if labels.is_empty() {
        return Ok(0);
    }
    let current: HashMap<String, Option<String>> =
        sqlx::query("SELECT pc_id, nats_user FROM agents")
            .fetch_all(pool)
            .await
            .context("read current nats_user labels")?
            .into_iter()
            .map(|r| {
                (
                    r.try_get::<String, _>("pc_id").unwrap_or_default(),
                    r.try_get::<Option<String>, _>("nats_user").ok().flatten(),
                )
            })
            .collect();
    let pending: Vec<(&String, &String)> = labels
        .iter()
        .filter(|(pc_id, label)| match current.get(pc_id.as_str()) {
            // No row for this pc_id — the UPDATE below would match nothing
            // anyway, and must not create one: the pc_id half of a
            // connection is claimed by the host, not proved.
            None => false,
            Some(existing) => existing.as_deref() != Some(label.as_str()),
        })
        .collect();
    if pending.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await.context("begin nats_user tx")?;
    let mut changed = 0;
    // The `WHERE` guard stays: the row can move between the read above and
    // the write below, and it is what keeps `nats_user_since` honest.
    for (pc_id, label) in pending {
        let res = sqlx::query(
            "UPDATE agents
                SET nats_user = ?, nats_user_since = CURRENT_TIMESTAMP
              WHERE pc_id = ?
                AND (nats_user IS NULL OR nats_user <> ?)",
        )
        .bind(label)
        .bind(pc_id)
        .bind(label)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("update nats_user for {pc_id}"))?;
        changed += res.rows_affected();
    }
    tx.commit().await.context("commit nats_user tx")?;
    Ok(changed)
}

/// A lost poll must invalidate current proof, rather than preserve a green
/// historical credential. The API also enforces the persisted 90 s deadline.
async fn invalidate_identity_proofs(pool: &SqlitePool) -> Result<()> {
    sqlx::query("UPDATE agents SET nats_user = 'unknown', nats_user_since = CURRENT_TIMESTAMP, nats_identity_expires_at = NULL WHERE nats_identity_expires_at IS NOT NULL")
        .execute(pool).await?;
    Ok(())
}

async fn apply_identity_projection(
    pool: &SqlitePool,
    conns: &[ConnInfo],
    probe: &CredentialProbe,
    ev: Evidence,
) -> Result<u64> {
    let (labels, _) = correlate(conns, probe, ev);
    let mut latest = HashMap::<String, &ConnInfo>::new();
    for c in conns {
        if let Some(parsed) = c.name.as_deref().and_then(parse_client_name)
            && parsed.role == NatsRole::Agent.as_str()
            && let Some(pc) = parsed.identity
            && latest.get(pc).is_none_or(|old| old.cid < c.cid)
        {
            latest.insert(pc.to_owned(), c);
        }
    }
    let hosts: Vec<String> = latest
        .into_iter()
        .filter_map(|(pc, c)| {
            c.identity_label
                .as_deref()
                .filter(|n| public_identity(n))
                .filter(|n| labels.get(&pc).is_some_and(|label| label == *n))
                .map(|_| pc)
        })
        .collect();
    let json = serde_json::to_string(&hosts)?;
    sqlx::query("UPDATE agents SET nats_user = 'unknown', nats_user_since = CURRENT_TIMESTAMP, nats_identity_expires_at = NULL WHERE nats_identity_expires_at IS NOT NULL AND pc_id NOT IN (SELECT value FROM json_each(?))")
        .bind(&json).execute(pool).await?;
    let changed = apply(pool, &labels).await?;
    sqlx::query("UPDATE agents SET nats_identity_expires_at = ? WHERE pc_id IN (SELECT value FROM json_each(?))")
        .bind(Utc::now() + chrono::Duration::seconds(90)).bind(json).execute(pool).await?;
    Ok(changed)
}

/// An HTTP client for the monitoring endpoint.
///
/// Plain HTTP, deliberately: `http_port` speaks no TLS, and giving the
/// backend a TLS-capable client here would pull a second rustls crypto
/// provider into the binary — the exact ambiguity that made `kanade run`
/// panic on its first flush in #1187, except it would land on the SMTP
/// path instead.
type MonitorClient =
    Client<hyper_util::client::legacy::connect::HttpConnector, http_body_util::Empty<Bytes>>;

async fn fetch_page(client: &MonitorClient, base: &str, offset: usize) -> Result<Connz> {
    // `auth=1` is what makes the endpoint report `authorized_user` at all;
    // `subs=0` keeps subscription lists (by far the largest part of a
    // connection record) off the wire.
    let url = format!("{base}/connz?auth=1&subs=0&sort=cid&limit={PAGE_SIZE}&offset={offset}");
    fetch_json(client, &url).await
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &MonitorClient,
    url: &str,
) -> Result<T> {
    let uri: hyper::Uri = url.parse().with_context(|| format!("parse {url}"))?;
    let res = client
        .get(uri)
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = res.status();
    if !status.is_success() {
        bail!("GET {url} returned HTTP {status}");
    }
    // `Limited` aborts the read past the cap rather than checking the size
    // after the fact — the point is to never buffer it, not to notice
    // afterwards that we did.
    let body = http_body_util::Limited::new(res.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("read body of {url} (cap {MAX_BODY_BYTES} bytes)"))?
        .to_bytes();
    serde_json::from_slice(&body).map_err(|e| {
        // Deserializer errors can embed a raw value from an invalid field.
        // Keep response content and credentials out of logs.
        anyhow::anyhow!(
            "invalid monitoring response from {url} at line {}, column {}",
            e.line(),
            e.column()
        )
    })
}

/// Walk every page of `/connz`. The flag says whether the walk covered the
/// broker's own `total`: a walk cut short by the page cap, or by a broker
/// that stopped serving, is not the full picture, and a caller reasoning from
/// absence ("no such connection any more") must know that.
async fn fetch_walk(client: &MonitorClient, base: &str) -> Result<(Vec<ConnInfo>, bool, Varz)> {
    let before: Varz = fetch_json(client, &format!("{base}/varz")).await?;
    if before.server_id.is_empty()
        || before.server_id.len() > 64
        || !before.server_id.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        bail!("invalid broker identifier");
    }
    let mut out: Vec<ConnInfo> = Vec::new();
    let mut ids = HashSet::new();
    let mut complete = false;
    let mut consistent = true;
    let mut last_now = before.now;
    for _ in 0..MAX_PAGES {
        // CID order is ascending and new connections append. Re-read the
        // preceding boundary row: prefix removals would shift offsets and
        // otherwise silently skip unseen connections. New arrivals alone do
        // not invalidate absence evidence for the pages already covered.
        let anchor = out.last().map(|c| c.cid);
        let offset = out.len().saturating_sub(1);
        let z = fetch_page(client, base, offset).await?;
        let observed_at = Utc::now();
        let at = z.now.context("connz missing timestamp")?;
        if z.server_id != before.server_id || at < last_now || at < before.start {
            bail!("broker identity or time changed during connz walk");
        }
        last_now = at;
        if z.offset != Some(offset) || z.num_connections != Some(z.connections.len()) {
            bail!("connz page metadata missing or inconsistent");
        }
        let got = z.connections.len();
        let end = offset
            .checked_add(got)
            .context("connz page range overflow")?;
        consistent &= end <= z.total;
        if let Some(cid) = anchor {
            consistent &= z.connections.first().is_some_and(|c| c.cid == cid);
        }
        let mut previous = anchor.unwrap_or(0);
        for (index, mut c) in z.connections.into_iter().enumerate() {
            if index == 0 && anchor == Some(c.cid) {
                continue;
            }
            if c.cid <= previous || !ids.insert(c.cid) {
                consistent = false;
                continue;
            }
            previous = c.cid;
            c.acceptance = before.acceptance(&c, at);
            c.identity_label = verified_identity(&c, &before, observed_at);
            c.connection = Some(super::nats_auth_evidence::Connection {
                observed_at: Some(observed_at),
                cid: c.cid.to_string(),
                server_id: before.server_id.clone(),
                broker_start: before.start,
                broker_now: Some(at),
                connection_start: c.start,
                last_activity: c.last_activity,
            });
            out.push(c);
        }
        if got <= usize::from(anchor.is_some()) || end >= z.total || !consistent {
            complete = end == z.total && consistent;
            break;
        }
    }
    let after: Varz = fetch_json(client, &format!("{base}/varz")).await?;
    if !before.same_epoch(&after) || last_now > after.now {
        bail!("broker restarted or reloaded during monitoring poll");
    }
    Ok((out, complete, before))
}

/// Walk every page of `/connz` and return the connections.
#[cfg(test)]
async fn fetch_all(client: &MonitorClient, base: &str) -> Result<Vec<ConnInfo>> {
    Ok(fetch_walk(client, base).await?.0)
}

/// Hand one poll to the authentication audit. Failures are logged, never
/// fatal: the audit is a second reader of this poll, and must not stop the
/// credential projection that shares it.
async fn audit_authentication(
    pool: &SqlitePool,
    js: &async_nats::jetstream::Context,
    observed: &[Observed],
    complete: bool,
    reasons: &BTreeMap<EvidenceReason, usize>,
) {
    let registered = match nats_auth_audit::registered_pcs(pool).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "nats auth audit skipped: agents unreadable");
            return;
        }
    };
    let settings = nats_auth_audit::read_settings(js).await;
    let poll = nats_auth_audit::Poll {
        observed,
        registered: &registered,
        complete,
        uncertainty_reasons: Some(reasons),
    };
    match nats_auth_audit::run_poll(pool, settings, &poll, Utc::now()).await {
        Ok(0) => {}
        Ok(written) => info!(written, "nats auth audit notices updated"),
        Err(e) => warn!(error = %format!("{e:#}"), "nats auth audit failed"),
    }
}

/// Poll the broker forever. Never returns in normal operation.
///
/// Failure to reach the monitoring endpoint is not fatal and not even
/// especially unusual (monitoring can be off, or bound elsewhere): the
/// column simply stays NULL, which reads as "never correlated" — the honest
/// answer. The log says so once per state change rather than once a minute,
/// so a permanently-disabled endpoint doesn't drown the log while a
/// *newly* broken one is still visible.
///
/// The backend's live connection, server id and cid must match the monitored
/// TCP listener before its credential can prove the broker's mode. A resolved
/// credential alone is not evidence — see [`Evidence`].
pub async fn run(
    pool: SqlitePool,
    monitor_url: String,
    nats: async_nats::Client,
    js: async_nats::jetstream::Context,
) -> Result<()> {
    super::nats_auth_evidence::prune(&pool, Utc::now()).await?;
    let probe = CredentialProbe::for_role(NatsRole::Backend);
    let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
    info!(
        monitor_url = %monitor_url,
        poll_secs = POLL_INTERVAL.as_secs(),
        "nats connections projector started",
    );
    let mut healthy: Option<bool> = None;
    // Last poll's uncorrelatable connections, so a standing anomaly is
    // reported when it appears and when it changes — not every minute for
    // as long as the host stays misconfigured.
    let mut reported = Anomalies::default();
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // Dropping the `fetch_walk` future on timeout cancels the in-flight
        // request and closes the connection, so a wedged endpoint costs one
        // poll rather than the projector.
        let polled = match tokio::time::timeout(POLL_TIMEOUT, fetch_walk(&client, &monitor_url))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "poll timed out after {POLL_TIMEOUT:?} (endpoint accepted the connection but did \
                 not finish answering)"
            )),
        };
        match polled {
            Ok((conns, complete, broker)) => {
                let seen = conns.len();
                // Re-read per poll: the backend's link can drop and come
                // back, and the proof is only as current as the connection.
                let backend = nats.server_info();
                let ev = monitor_evidence(
                    &probe,
                    nats.connection_state() == async_nats::connection::State::Connected,
                    &backend.server_id,
                    backend.client_id,
                    &broker,
                    &conns,
                );
                let (labels, anomalies) = correlate(&conns, &probe, ev);
                audit_authentication(
                    &pool,
                    &js,
                    &observe(&conns, &probe, ev),
                    complete,
                    &uncertainty_reasons(&conns),
                )
                .await;
                let correlated = labels.len();
                if anomalies != reported {
                    if !anomalies.non_agent.is_empty() {
                        warn!(
                            connections = anomalies.non_agent.len(),
                            "NATS connections name a pc_id under a non-agent role; ignored",
                        );
                    }
                    if !anomalies.conflicting.is_empty() {
                        warn!(
                            hosts = anomalies.conflicting.len(),
                            "two live NATS connections claim one pc_id with different credentials",
                        );
                    }
                    if anomalies.is_empty() {
                        info!("previously reported NATS connection anomalies have cleared");
                    }
                    reported = anomalies;
                }
                match apply_identity_projection(&pool, &conns, &probe, ev).await {
                    Ok(changed) => {
                        if healthy != Some(true) {
                            info!(
                                monitor_url = %monitor_url,
                                connections = seen,
                                correlated,
                                "reading NATS connection credentials",
                            );
                            healthy = Some(true);
                        }
                        if changed > 0 {
                            info!(changed, correlated, "agent NATS credentials updated");
                        } else {
                            debug!(correlated, "agent NATS credentials unchanged");
                        }
                    }
                    Err(e) => warn!(error = %format!("{e:#}"), "nats_user projection write failed"),
                }
            }
            Err(e) => {
                if invalidate_identity_proofs(&pool).await.is_err() {
                    warn!("NATS identity proof invalidation failed");
                }
                // The audit's own answer is now unknown, not "clean": say so
                // in its recorded state and leave the ledger as it was.
                if let Err(se) =
                    nats_auth_audit::record_state(&pool, PollStatus::EndpointUnreadable, Utc::now())
                        .await
                {
                    warn!(error = %format!("{se:#}"), "record nats auth audit state failed");
                }
                if healthy != Some(false) {
                    warn!(
                        error = %format!("{e:#}"),
                        monitor_url = %monitor_url,
                        "NATS monitoring endpoint unreadable; agent NATS credentials will not be \
                         updated until it recovers",
                    );
                    healthy = Some(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// The literal shape the monitoring endpoint returns, so the decode is
    /// tested against the real field names rather than a hand-built struct.
    fn parse(json: &str) -> Connz {
        let mut z: Connz = serde_json::from_str(json).expect("decode /connz");
        // Legacy credential fixtures describe standard TCP clients.
        for c in &mut z.connections {
            c.transport.get_or_insert_with(|| "nats".into());
        }
        z
    }

    /// A probe around a known credential. Deliberately NOT
    /// `CredentialProbe::for_role`: that reads the registry, so on a
    /// developer's own fleet host it would resolve the REAL token and make
    /// these assertions depend on the machine they run on.
    fn probe_holding(token: Option<&str>) -> CredentialProbe {
        CredentialProbe::from_token(token.map(str::to_string))
    }

    #[test]
    fn a_token_authenticated_connection_is_the_shared_token() {
        // What the fleet looks like today: nats-server hides the credential
        // and reports only that one was used. That is still the answer an
        // operator needs -- "this host is on the shared token" -- because a
        // broker accepting token auth has exactly one token.
        let probe = probe_holding(Some("fleet-secret"));
        let ev = Evidence::TokenMode;
        assert_eq!(classify(Some("[REDACTED]"), &probe, ev), LABEL_SHARED_TOKEN);
        // ...and the same conclusion when the broker reports the value
        // instead of hiding it, which is not behaviour we control.
        assert_eq!(
            classify(Some("fleet-secret"), &probe, ev),
            LABEL_SHARED_TOKEN,
        );
        // The redaction marker is recognised even with nothing proven --
        // only a token-mode broker emits it.
        assert_eq!(
            classify(Some("[REDACTED]"), &probe_holding(None), Evidence::Unproven),
            LABEL_SHARED_TOKEN,
        );
    }

    #[test]
    fn a_credential_we_cannot_vouch_for_is_never_stored_verbatim() {
        // In token mode any value that is neither the redaction marker nor
        // our own credential is somebody's secret. Recording it would put
        // the fleet's NATS credential in the API response.
        let probe = probe_holding(Some("fleet-secret"));
        let out = classify(Some("some-other-secret"), &probe, Evidence::TokenMode);
        assert_eq!(out, LABEL_UNKNOWN);
        assert!(!out.contains("secret"), "{out}");
    }

    #[test]
    fn a_username_is_kept_only_against_positive_proof_of_users_mode() {
        // Proof = our own live connection authenticated as a user, which a
        // token-mode broker would have rejected. Then the reported value is
        // a username and safe to record.
        let probe = CredentialProbe::from_user("kanade-backend");
        assert_eq!(
            classify(Some("kanade-agent"), &probe, Evidence::UsersMode),
            "kanade-agent",
        );
        // Absence of a local credential is NOT that proof (review #1273,
        // claude): this projector polls plain HTTP and keeps running when
        // the backend's own connection is down, so "we hold no token" can
        // just as easily mean "this host is misconfigured" -- and a
        // non-redacting token-mode broker would then have its credential
        // stored and served verbatim.
        assert_eq!(
            classify(
                Some("fleet-secret"),
                &probe_holding(None),
                Evidence::Unproven
            ),
            LABEL_UNKNOWN,
        );
        // Same for a user-credentialled backend whose link is down: the
        // credential is only evidence while the broker is accepting it.
        assert_eq!(
            classify(Some("fleet-secret"), &probe, Evidence::Unproven),
            LABEL_UNKNOWN,
        );
    }

    #[test]
    fn observe_keeps_every_kanade_or_nameless_connection_and_only_safe_labels() {
        let secret = "tok-9f8e7d6c5b4a-SECRET";
        let z = parse(&format!(
            r#"{{"total":5,"connections":[
                {{"cid":1,"name":"kanade-agent/PC1","authorized_user":"{secret}"}},
                {{"cid":2,"name":"kanade-backend","authorized_user":"[REDACTED]"}},
                {{"cid":3,"authorized_user":""}},
                {{"cid":4,"name":"kanade-cli","authorized_user":"fleet-secret"}},
                {{"cid":5,"name":"auth-probe","authorized_user":"x"}}
            ]}}"#
        ));
        let probe = probe_holding(Some("fleet-secret"));
        let out = observe(&z.connections, &probe, Evidence::TokenMode);
        let labels: Vec<_> = out.iter().map(|o| (o.claimed, o.label.as_str())).collect();
        assert_eq!(
            labels,
            [
                (Claimed::Role(NatsRole::Agent), LABEL_UNKNOWN),
                (Claimed::Role(NatsRole::Backend), LABEL_SHARED_TOKEN),
                (Claimed::Unnamed, LABEL_UNPROVEN),
                (Claimed::Role(NatsRole::Cli), LABEL_SHARED_TOKEN),
            ],
            "the probe connection is not ours to judge",
        );
        assert_eq!(out[0].pc_id.as_deref(), Some("PC1"));
        assert!(!format!("{out:?}").contains(secret));
    }

    #[test]
    fn evidence_comes_from_the_live_connection_not_the_resolved_credential() {
        // The distinction the finding turned on: a resolved credential says
        // what this host WOULD present; only an accepted one says what the
        // broker accepts.
        let token = probe_holding(Some("fleet-secret"));
        let user = CredentialProbe::from_user("kanade-backend");
        let none = probe_holding(None);
        assert_eq!(evidence(&token, true), Evidence::TokenMode);
        assert_eq!(evidence(&user, true), Evidence::UsersMode);
        // Connected while presenting nothing says the broker required
        // nothing of US -- nothing about how it authenticated anyone else.
        assert_eq!(evidence(&none, true), Evidence::Unproven);
        for p in [&token, &user, &none] {
            assert_eq!(
                evidence(p, false),
                Evidence::Unproven,
                "a credential proves nothing while the link is down",
            );
        }
    }

    #[test]
    fn missing_user_does_not_prove_no_auth() {
        let probe = probe_holding(None);
        for reported in [None, Some(""), Some("   ")] {
            assert_eq!(
                classify(reported, &probe, Evidence::Unproven),
                LABEL_UNPROVEN,
                "{reported:?} alone proves nothing about acceptance",
            );
        }
        // Holding a token cannot turn missing broker data into acceptance.
        let probe = probe_holding(Some("fleet-secret"));
        assert_eq!(classify(None, &probe, Evidence::TokenMode), LABEL_UNPROVEN);
    }

    #[test]
    fn only_agent_connections_carrying_a_pc_id_are_correlated() {
        let probe = probe_holding(Some("fleet-secret"));
        let z = parse(
            r#"{"total":5,"connections":[
                {"cid":1,"name":"kanade-agent/PC001","authorized_user":"[REDACTED]"},
                {"cid":2,"name":"kanade-backend","authorized_user":"[REDACTED]"},
                {"cid":3,"name":"kanade-agent","authorized_user":"[REDACTED]"},
                {"cid":4,"name":"NATS CLI Version 0.1.5","authorized_user":"[REDACTED]"},
                {"cid":5,"authorized_user":"[REDACTED]"}
            ]}"#,
        );
        let (out, anomalies) = correlate(&z.connections, &probe, Evidence::TokenMode);
        assert_eq!(out.len(), 1, "only the named agent connection: {out:?}");
        assert!(anomalies.is_empty(), "{anomalies:?}");
        assert_eq!(
            out.get("PC001").map(String::as_str),
            Some(LABEL_SHARED_TOKEN)
        );
        // An agent too old to announce its pc_id must NOT be attributed to
        // anything -- it stays "never correlated", which is true.
        assert!(!out.contains_key("kanade-agent"));
    }

    #[test]
    fn the_newest_connection_wins_when_two_claim_one_host() {
        // A reconnect can leave the old connection briefly open, and the
        // stale one must not be what gets recorded.
        // A `users`-mode broker, so the reported usernames are storable and
        // the tie-break is visible in the stored value.
        let probe = CredentialProbe::from_user("kanade-backend");
        let z = parse(
            r#"{"total":2,"connections":[
                {"cid":7,"name":"kanade-agent/PC001","authorized_user":"legacy"},
                {"cid":9,"name":"kanade-agent/PC001","authorized_user":"kanade-agent"}
            ]}"#,
        );
        let (out, _) = correlate(&z.connections, &probe, Evidence::UsersMode);
        assert_eq!(out.get("PC001").map(String::as_str), Some("kanade-agent"));
        // ...regardless of the order the broker listed them in.
        let z = parse(
            r#"{"total":2,"connections":[
                {"cid":9,"name":"kanade-agent/PC001","authorized_user":"kanade-agent"},
                {"cid":7,"name":"kanade-agent/PC001","authorized_user":"legacy"}
            ]}"#,
        );
        let (out, _) = correlate(&z.connections, &probe, Evidence::UsersMode);
        assert_eq!(out.get("PC001").map(String::as_str), Some("kanade-agent"));
    }

    /// Serve `pages` canned `/connz` bodies in order, recording the query
    /// string of each request. Enough HTTP for hyper, and no more.
    async fn canned_connz(
        pages: Vec<String>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let mut offset: usize = 0;
        let mut responses = vec![varz_fixture()];
        for page in pages {
            let mut page: serde_json::Value = serde_json::from_str(&page).unwrap();
            page["server_id"] = "TESTSERVER".into();
            page["now"] = "2026-10-18T02:24:58Z".into();
            page["offset"] = offset.into();
            let len = page["connections"].as_array().unwrap().len();
            page["num_connections"] = len.into();
            offset = (offset + len).saturating_sub(1);
            responses.push(page.to_string());
        }
        responses.push(varz_fixture());
        canned_http(responses).await
    }

    fn varz_fixture() -> String {
        r#"{"server_id":"TESTSERVER","start":"2026-10-18T02:00:00Z",
        "now":"2026-10-18T02:24:58Z","config_load_time":"2026-10-18T02:00:00Z",
        "total_connections":10,"auth_timeout":2,"auth_required":true}"#
            .into()
    }

    async fn canned_http(
        pages: Vec<String>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let queries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = queries.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for body in pages {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(line) = req.lines().next() {
                    seen.lock().unwrap().push(line.to_string());
                }
                let res = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = sock.write_all(res.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://127.0.0.1:{port}"), queries)
    }

    #[tokio::test]
    async fn coherent_snapshot_captures_anonymous_id_without_raw_secrets() {
        let secret = "must-not-appear-in-evidence";
        let epoch = varz_fixture().replace("TESTSERVER", "BROKER1");
        let page = format!(
            r#"{{"server_id":"BROKER1","now":"2026-10-18T02:24:58Z","offset":0,"num_connections":1,"total":1,"connections":[{{"cid":18446744073709551615,"authorized_user":"{secret}","ip":"127.0.0.1","start":"2026-10-18T02:24:57Z"}}]}}"#
        );
        let (base, _) = canned_http(vec![epoch.clone(), page, epoch]).await;
        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let (conns, complete, _) = fetch_walk(&client, &base).await.unwrap();
        let obs = observe(&conns, &probe_holding(None), Evidence::Unproven);
        assert!(complete);
        assert_eq!(obs.len(), 1);
        let c = obs[0].connection.as_ref().unwrap();
        assert_eq!(c.cid, u64::MAX.to_string());
        assert_eq!(c.server_id, "BROKER1");
        assert!(c.connection_start.is_some());
        let json = serde_json::to_string(c).unwrap();
        assert!(!json.contains(secret));
        assert!(!json.contains("127.0.0.1"));
        assert_eq!(obs[0].label, LABEL_UNKNOWN);
    }

    #[tokio::test]
    async fn restart_missing_identity_and_inconsistent_pages_cannot_be_complete() {
        let epoch = varz_fixture().replace("TESTSERVER", "BROKER1");
        let page = r#"{"server_id":"BROKER1","now":"2026-10-18T02:24:58Z","offset":0,"num_connections":1,"total":1,"connections":[{"cid":1}]}"#.to_string();
        let restarted = epoch.replace("02:00:00Z", "02:24:59Z");
        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        for pages in [
            vec![epoch.clone(), page.clone(), restarted],
            vec!["{}".into()],
            vec![epoch.clone(), page.replace("BROKER1", "BROKER2")],
            vec![
                epoch.clone(),
                page.replace("\"total\":1", "\"total\":2"),
                page.replace("\"offset\":0", "\"offset\":1"),
            ],
        ] {
            let (base, _) = canned_http(pages).await;
            assert!(fetch_walk(&client, &base).await.is_err());
        }
    }

    #[tokio::test]
    async fn snapshot_page_cap_reports_incomplete_without_losing_sampled_ids() {
        let epoch = varz_fixture().replace("TESTSERVER", "BROKER1");
        let mut pages = vec![epoch.clone()];
        for cid in 1..=MAX_PAGES {
            let offset = cid.saturating_sub(2);
            let conns = if cid == 1 {
                serde_json::json!([{"cid":cid}])
            } else {
                serde_json::json!([{"cid":cid-1},{"cid":cid}])
            };
            pages.push(
                serde_json::json!({
                    "server_id":"BROKER1","now":"2026-10-18T02:24:58Z",
                    "offset":offset,"num_connections":conns.as_array().unwrap().len(),
                    "total":1000,"connections":conns
                })
                .to_string(),
            );
        }
        pages.push(epoch);
        let (base, _) = canned_http(pages).await;
        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let (conns, complete, _) = fetch_walk(&client, &base).await.unwrap();
        assert!(!complete);
        assert_eq!(conns.len(), MAX_PAGES);
        assert_eq!(
            conns.last().unwrap().connection.as_ref().unwrap().cid,
            MAX_PAGES.to_string()
        );
    }

    #[tokio::test]
    async fn invalid_monitor_response_errors_do_not_echo_credentials() {
        let (base, _) = canned_http(vec![r#"{"total":"secret-must-not-be-logged"}"#.into()]).await;
        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let err = fetch_page(&client, &base, 0).await.unwrap_err();
        assert!(!format!("{err:#}").contains("secret-must-not-be-logged"));
    }

    /// A fleet larger than one `/connz` page has to be walked, and a walk
    /// that stops early is the failure this projection cannot have: the
    /// hosts in the tail would read as "never correlated" — indistinguishable
    /// from a host nobody can account for — with nothing in the log to say
    /// they were simply never fetched.
    #[tokio::test]
    async fn a_fleet_larger_than_one_page_is_walked_to_the_end() {
        let page = |cids: &[u64]| {
            let conns: Vec<String> = cids
                .iter()
                .map(|c| {
                    format!(
                        r#"{{"cid":{c},"name":"kanade-agent/PC{c:03}","authorized_user":"[REDACTED]"}}"#
                    )
                })
                .collect();
            format!(r#"{{"total":3,"connections":[{}]}}"#, conns.join(","))
        };
        let (base, queries) = canned_connz(vec![page(&[1, 2]), page(&[2, 3])]).await;
        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();

        let conns = fetch_all(&client, &base).await.expect("walk both pages");
        assert_eq!(conns.len(), 3, "the tail of the fleet must not be dropped");

        // The second request must resume where the first stopped, or the
        // walk silently re-reads page one until MAX_PAGES.
        let queries: Vec<_> = queries
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("/connz?"))
            .cloned()
            .collect();
        assert_eq!(queries.len(), 2, "{queries:?}");
        assert!(queries[0].contains("offset=0"), "{}", queries[0]);
        assert!(queries[1].contains("offset=1"), "{}", queries[1]);
        // And `auth=1` is what makes the endpoint report the credential at
        // all — without it every connection would look unauthenticated.
        assert!(queries.iter().all(|q| q.contains("auth=1")), "{queries:?}");
    }

    /// A broker that claims a `total` it will not serve must not spin the
    /// walk. The short page ends it.
    #[tokio::test]
    async fn a_total_the_broker_will_not_serve_still_terminates() {
        let (base, queries) = canned_connz(vec![
            r#"{"total":9999,"connections":[{"cid":1,"name":"kanade-agent/PC001"}]}"#.to_string(),
            r#"{"total":9999,"connections":[]}"#.to_string(),
        ])
        .await;
        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let conns = fetch_all(&client, &base).await.expect("walk terminates");
        assert_eq!(conns.len(), 1);
        assert_eq!(
            queries
                .lock()
                .unwrap()
                .iter()
                .filter(|q| q.contains("/connz?"))
                .count(),
            2,
            "an empty page ends the walk"
        );
    }

    /// A monitoring port that accepts the connection and then says nothing —
    /// a hung nats-server, another process squatting the port, a firewall
    /// that blackholes established connections.
    ///
    /// The hazard is that `hyper_util`'s client has no timeout of its own,
    /// so the poll would never return and the loop would stop ticking
    /// *silently* (review #1273, coderabbit). This asserts the request is
    /// actually cancellable — that the wrapper in `run` has something to
    /// cancel, rather than a future stuck in a blocking read.
    #[tokio::test]
    async fn a_hung_endpoint_loses_one_poll_rather_than_the_projector() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept and hold: never write a response, never close.
        let _accepting = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let base = format!("http://127.0.0.1:{port}");
        let out = tokio::time::timeout(Duration::from_millis(300), fetch_all(&client, &base)).await;
        assert!(
            out.is_err(),
            "a hung endpoint must be interruptible by the caller's deadline, not answer",
        );
    }

    async fn pool_with(pcs: &[&str]) -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for pc in pcs {
            sqlx::query("INSERT INTO agents (pc_id) VALUES (?)")
                .bind(pc)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    async fn read(pool: &SqlitePool, pc: &str) -> (Option<String>, Option<String>) {
        let r = sqlx::query("SELECT nats_user, nats_user_since FROM agents WHERE pc_id = ?")
            .bind(pc)
            .fetch_one(pool)
            .await
            .unwrap();
        (r.try_get(0).unwrap(), r.try_get(1).unwrap())
    }

    #[tokio::test]
    async fn a_host_that_was_never_seen_stays_null() {
        let pool = pool_with(&["PC001", "PC002"]).await;
        let labels = HashMap::from([("PC001".to_string(), LABEL_SHARED_TOKEN.to_string())]);
        assert_eq!(apply(&pool, &labels).await.unwrap(), 1);
        // The distinction the column exists for: "on the shared token" is
        // the migration queue, "never correlated" is a host nobody can
        // account for. Collapsing them would make the first uncountable.
        assert_eq!(
            read(&pool, "PC001").await.0.as_deref(),
            Some(LABEL_SHARED_TOKEN)
        );
        assert_eq!(read(&pool, "PC002").await.0, None);
    }

    #[tokio::test]
    async fn an_unchanged_label_is_not_rewritten() {
        let pool = pool_with(&["PC001"]).await;
        let labels = HashMap::from([("PC001".to_string(), LABEL_SHARED_TOKEN.to_string())]);
        apply(&pool, &labels).await.unwrap();
        let (_, first_since) = read(&pool, "PC001").await;
        assert!(first_since.is_some());
        // A second poll over an unchanged fleet must write nothing at all —
        // both to keep the writer idle and so `since` keeps meaning "since
        // when", not "as of the last poll".
        assert_eq!(apply(&pool, &labels).await.unwrap(), 0);
        assert_eq!(read(&pool, "PC001").await.1, first_since);
    }

    #[tokio::test]
    async fn moving_to_a_new_credential_restamps_since() {
        let pool = pool_with(&["PC001"]).await;
        apply(
            &pool,
            &HashMap::from([("PC001".to_string(), LABEL_SHARED_TOKEN.to_string())]),
        )
        .await
        .unwrap();
        // Backdate the stamp before the second write: CURRENT_TIMESTAMP has
        // 1 s resolution, so re-reading "now" would be indistinguishable
        // from no update at all within the same second.
        sqlx::query(
            "UPDATE agents SET nats_user_since = '2000-01-01 00:00:00' WHERE pc_id='PC001'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            apply(
                &pool,
                &HashMap::from([("PC001".to_string(), "kanade-agent".to_string())]),
            )
            .await
            .unwrap(),
            1,
        );
        let (user, after) = read(&pool, "PC001").await;
        assert_eq!(user.as_deref(), Some("kanade-agent"));
        assert_ne!(
            after.as_deref(),
            Some("2000-01-01 00:00:00"),
            "a credential change must restamp `since`",
        );
    }

    /// Everything above tests our reasoning about `/connz`. This tests the
    /// reasoning itself, against a real broker:
    ///
    ///   * what `authorized_user` holds under token auth — the premise the
    ///     classifier is built on, and the thing this test caught being
    ///     different from what the docs implied;
    ///   * the connection name survives the round trip, so the pc_id join
    ///     works;
    ///   * `fetch_all` can actually read and decode the live endpoint.
    ///
    /// If a future nats-server changes any of those, this fails loudly
    /// instead of the projector quietly recording `unknown` for the whole
    /// fleet — or, worse, quietly recording a credential.
    ///
    /// Ignored by default (needs `nats-server` in PATH), like the
    /// kv_cas_live suite:
    ///
    /// ```text
    /// cargo test -p kanade-backend -- --ignored connz
    /// ```
    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn live_connz_reports_the_token_as_the_user_and_echoes_our_name() {
        use std::io::Write as _;

        const TOKEN: &str = "live-test-fleet-token";
        let client_port = portpicker::pick_unused_port().expect("pick client port");
        let http_port = portpicker::pick_unused_port().expect("pick monitor port");
        let dir = tempfile::TempDir::new().expect("tempdir");
        let conf = dir.path().join("nats.conf");
        let mut f = std::fs::File::create(&conf).expect("write conf");
        // Token auth + monitoring: the shape configs/nats-server.conf ships.
        write!(
            f,
            "port: {client_port}\nhttp_port: {http_port}\nauthorization {{ token: \"{TOKEN}\" }}\n"
        )
        .expect("write conf");
        drop(f);
        let _server = tokio::process::Command::new("nats-server")
            .arg("-c")
            .arg(&conf)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server (is it in PATH?)");

        // Announce ourselves exactly as an agent does. Built through the
        // shared helper so a change to the name format breaks here too.
        let name = kanade_shared::nats_client::client_name(NatsRole::Agent, Some("PC-LIVE"));
        let url = format!("nats://127.0.0.1:{client_port}");
        let mut conn = None;
        for _ in 0..50 {
            match async_nats::ConnectOptions::new()
                .token(TOKEN.to_string())
                .name(name.clone())
                .connect(&url)
                .await
            {
                Ok(c) => {
                    conn = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let _conn = conn.expect("nats-server did not come up in 5s");

        let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let base = format!("http://127.0.0.1:{http_port}");
        let (conns, complete, _) = fetch_walk(&http, &base)
            .await
            .expect("read live broker snapshot");
        assert!(complete);
        let ours = conns
            .iter()
            .find(|c| c.name.as_deref() == Some(name.as_str()))
            .expect("our connection is in /connz under the name we announced");

        // The measured fact the module is built on: the broker hides the
        // credential and reports only that one was presented. Either way
        // the value must be one classify() recognises — the failure mode
        // being guarded against is a THIRD shape, which would silently
        // label the entire fleet `unknown`.
        let reported = ours
            .authorized_user
            .as_deref()
            .expect("a token-authenticated connection reports an authorized_user");
        assert!(
            reported == REDACTED_BY_BROKER || reported == TOKEN,
            "unexpected authorized_user shape under token auth; revisit classify()",
        );
        assert_eq!(
            reported, REDACTED_BY_BROKER,
            "nats-server {} redacts it; if a build stops doing that, the classifier still \
             holds (it compares against our own credential) but this assertion documents when \
             the behaviour changed",
            "2.14.3",
        );

        // ...and the projector turns that into a label, not a credential.
        let probe = probe_holding(Some(TOKEN));
        let (labels, _) = correlate(&conns, &probe, Evidence::TokenMode);
        assert_eq!(
            labels.get("PC-LIVE").map(String::as_str),
            Some(LABEL_SHARED_TOKEN),
        );
        assert!(
            !labels.values().any(|v| v.contains(TOKEN)),
            "the token must never reach a stored value: {labels:?}",
        );
    }

    #[tokio::test]
    async fn a_connection_for_an_unknown_pc_id_creates_no_row() {
        // The pc_id half of a connection is claimed by the host, not proved.
        // A claim about a machine the fleet has never registered must not
        // conjure one into the roster.
        let pool = pool_with(&["PC001"]).await;
        let labels = HashMap::from([("GHOST".to_string(), LABEL_SHARED_TOKEN.to_string())]);
        assert_eq!(apply(&pool, &labels).await.unwrap(), 0);
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM agents")
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
        assert_eq!(n, 1);
    }
    fn connection_fixture() -> ConnInfo {
        serde_json::from_str(
            r#"{"cid":1,"kind":"Client","type":"nats",
            "start":"2026-10-18T02:24:57Z","in_msgs":0,"out_msgs":0,
            "subscriptions":0}"#,
        )
        .unwrap()
    }

    fn snapshot_fixture() -> Varz {
        serde_json::from_str(&varz_fixture()).unwrap()
    }

    #[test]
    fn uncertainty_reasons_use_only_fixed_codes_and_ignore_authenticated_connections() {
        let v = snapshot_fixture();
        let mut pending = connection_fixture();
        pending.acceptance = v.acceptance(&pending, v.now);
        let mut missing = connection_fixture();
        missing.in_msgs = None;
        missing.acceptance = v.acceptance(&missing, v.now);
        let mut ws = connection_fixture();
        ws.transport = Some("websocket".into());
        ws.acceptance = v.acceptance(&ws, v.now);
        let mut expired = connection_fixture();
        expired.acceptance = v.acceptance(&expired, v.now + chrono::Duration::seconds(5));
        let mut authenticated = connection_fixture();
        authenticated.authorized_user = Some("secret-must-not-escape".into());
        let reasons = uncertainty_reasons(&[pending, missing, ws, expired, authenticated]);
        assert_eq!(
            reasons,
            BTreeMap::from([
                (EvidenceReason::PreConnectPending, 1),
                (EvidenceReason::IncompleteMonitoring, 1),
                (EvidenceReason::ListenerAuthUnproven, 1),
                (EvidenceReason::AcceptanceUnproven, 1),
            ])
        );
        let json = serde_json::to_string(&reasons).unwrap();
        assert!(json.contains("pre_connect_pending"));
        assert!(!json.contains("secret-must-not-escape"));
    }

    #[test]
    fn pre_connect_is_pending_and_expiry_is_unproven_never_no_auth() {
        let v = snapshot_fixture();
        let mut c = connection_fixture();
        let probe = probe_holding(None);
        for (at, expected) in [
            (v.now, LABEL_PENDING),
            (v.now + chrono::Duration::seconds(2), LABEL_UNPROVEN),
        ] {
            c.acceptance = v.acceptance(&c, at);
            assert_eq!(
                classify_connection(&c, &probe, Evidence::Unproven),
                expected
            );
        }
        // CONNECT metadata and RTT are not proof of successful authentication.
        c.name = Some("kanade-agent/PC1".into());
        c.acceptance = v.acceptance(&c, v.now);
        assert_eq!(
            classify_connection(&c, &probe, Evidence::Unproven),
            LABEL_PENDING
        );
        assert!(correlate(&[c], &probe, Evidence::Unproven).0.is_empty());
    }

    #[test]
    fn only_positive_application_activity_or_explicit_tcp_policy_proves_no_auth() {
        let mut v = snapshot_fixture();
        v.auth_required = None; // The real open broker omits its false flag.
        let probe = probe_holding(None);
        for transport in ["nats", "websocket"] {
            for field in ["in_msgs", "out_msgs", "subscriptions"] {
                let mut json = serde_json::json!({
                    "cid":1,"kind":"Client","type":transport,
                    "start":"2026-10-18T02:24:57Z","in_msgs":0,"out_msgs":0,"subscriptions":0
                });
                json[field] = 1.into();
                let mut c: ConnInfo = serde_json::from_value(json).unwrap();
                c.acceptance = v.acceptance(&c, v.now);
                assert_eq!(
                    classify_connection(&c, &probe, Evidence::Unproven),
                    if transport == "nats" {
                        LABEL_NO_AUTH
                    } else {
                        LABEL_UNPROVEN
                    }
                );
            }
        }
        let mut c = connection_fixture();
        v.auth_required = None;
        assert_eq!(v.acceptance(&c, v.now).label(), LABEL_UNPROVEN);
        v.auth_required = Some(false);
        assert_eq!(v.acceptance(&c, v.now).label(), LABEL_NO_AUTH);
        c.transport = Some("websocket".into());
        assert_eq!(v.acceptance(&c, v.now).label(), LABEL_UNPROVEN);
        c.subscriptions = Some(1);
        assert_eq!(v.acceptance(&c, v.now).label(), LABEL_UNPROVEN);
    }

    #[test]
    fn incomplete_connection_counters_or_listener_are_unproven() {
        let mut v = snapshot_fixture();
        v.auth_required = None;
        for missing in [
            "in_msgs",
            "out_msgs",
            "subscriptions",
            "kind",
            "type",
            "start",
        ] {
            let mut json = serde_json::json!({
                "cid":1,"kind":"Client","type":"nats",
                "start":"2026-10-18T02:24:57Z","in_msgs":0,"out_msgs":0,"subscriptions":1
            });
            let complete: ConnInfo = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(v.acceptance(&complete, v.now).label(), LABEL_NO_AUTH);
            json.as_object_mut().unwrap().remove(missing);
            let c: ConnInfo = serde_json::from_value(json).unwrap();
            assert_eq!(v.acceptance(&c, v.now).label(), LABEL_UNPROVEN, "{missing}");
        }
        assert!(serde_json::from_str::<Varz>("{}").is_err());
        assert!(serde_json::from_str::<Connz>("{}").is_err());
        assert!(serde_json::from_str::<Connz>(r#"{"total":0}"#).is_err());
    }

    #[test]
    fn anonymous_authenticated_connections_and_ws_overrides_remain_redacted() {
        let probe = CredentialProbe::from_user("backend");
        let mut c = connection_fixture();
        c.authorized_user = Some("agent".into());
        assert_eq!(
            classify_connection(&c, &probe, Evidence::UsersMode),
            "agent"
        );
        c.authorized_user = Some(REDACTED_BY_BROKER.into());
        assert_eq!(
            classify_connection(&c, &probe, Evidence::UsersMode),
            LABEL_SHARED_TOKEN
        );
        c.transport = Some("websocket".into());
        c.authorized_user = Some("different-listener-secret".into());
        assert_eq!(
            classify_connection(&c, &probe, Evidence::UsersMode),
            LABEL_UNKNOWN
        );
        c.authorized_user = Some(REDACTED_BY_BROKER.into());
        assert_eq!(
            classify_connection(&c, &probe, Evidence::UsersMode),
            LABEL_SHARED_TOKEN
        );
    }

    #[test]
    fn a_restart_reload_or_time_reversal_invalidates_the_epoch() {
        let v = snapshot_fixture();
        let mut next = snapshot_fixture();
        assert!(v.same_epoch(&next));
        next.server_id = "restarted".into();
        assert!(!v.same_epoch(&next));
        next = snapshot_fixture();
        next.start += chrono::Duration::seconds(1);
        assert!(!v.same_epoch(&next));
        next = snapshot_fixture();
        next.config_load_time += chrono::Duration::seconds(1);
        assert!(!v.same_epoch(&next));
        next = snapshot_fixture();
        next.now -= chrono::Duration::seconds(1);
        assert!(!v.same_epoch(&next));
    }

    #[tokio::test]
    async fn inconsistent_monitoring_never_proves_a_clean_poll() {
        let page = serde_json::json!({
            "server_id":"TESTSERVER","now":"2026-10-18T02:24:58Z",
            "offset":0,"num_connections":0,"total":0,"connections":[]
        });
        let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        for field in [
            "server_id",
            "offset",
            "num_connections",
            "now",
            "connections",
            "total",
        ] {
            let mut broken = page.clone();
            broken.as_object_mut().unwrap().remove(field);
            let (base, _) =
                canned_http(vec![varz_fixture(), broken.to_string(), varz_fixture()]).await;
            assert!(fetch_walk(&http, &base).await.is_err(), "{field}");
        }
        let mut restarted: serde_json::Value = serde_json::from_str(&varz_fixture()).unwrap();
        restarted["server_id"] = "restarted".into();
        let (base, _) = canned_http(vec![
            varz_fixture(),
            page.to_string(),
            restarted.to_string(),
        ])
        .await;
        assert!(fetch_walk(&http, &base).await.is_err());
        let mut churn: serde_json::Value = serde_json::from_str(&varz_fixture()).unwrap();
        churn["total_connections"] = 11.into();
        let (base, _) =
            canned_http(vec![varz_fixture(), page.to_string(), churn.to_string()]).await;
        assert!(fetch_walk(&http, &base).await.unwrap().1);
        let (base, _) = canned_http(vec!["{}".into()]).await;
        assert!(fetch_walk(&http, &base).await.is_err());
    }

    async fn disposable_broker(
        extra: &str,
    ) -> (tokio::process::Child, tempfile::TempDir, u16, String) {
        let port = portpicker::pick_unused_port().unwrap();
        let monitor = portpicker::pick_unused_port().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nats.conf");
        std::fs::write(
            &path,
            format!("host: 127.0.0.1\nport: {port}\nhttp_port: {monitor}\n{extra}\n"),
        )
        .unwrap();
        let mut server = tokio::process::Command::new("nats-server")
            .arg("-c")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("nats-server in PATH");
        let base = format!("http://127.0.0.1:{monitor}");
        let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        for _ in 0..50 {
            if fetch_json::<Varz>(&http, &format!("{base}/varz"))
                .await
                .is_ok()
            {
                return (server, dir, port, base);
            }
            assert!(
                server.try_wait().unwrap().is_none(),
                "disposable broker exited before ready"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("disposable broker did not start");
    }

    async fn raw_info(port: u16) -> (tokio::io::BufReader<tokio::net::TcpStream>, u64) {
        use tokio::io::AsyncBufReadExt;
        let mut socket = tokio::io::BufReader::new(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap(),
        );
        let mut line = String::new();
        socket.read_line(&mut line).await.unwrap();
        let info: serde_json::Value =
            serde_json::from_str(line.strip_prefix("INFO ").unwrap().trim()).unwrap();
        (socket, info["client_id"].as_u64().unwrap())
    }

    #[tokio::test]
    #[ignore = "requires disposable nats-server and loopback sockets"]
    async fn live_connz_pre_connect_rejection_anonymous_auth_and_reconnect() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(30), async {
            let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
            for users in [false, true] {
                let config = if users {
                    "authorization { timeout: 10, users: [{user: agent, password: test-password}] }"
                } else {
                    "authorization { timeout: 10, token: test-token }"
                };
                let (mut server, _dir, port, base) = disposable_broker(config).await;
                let mut previous = None;
                // A reconnect is a new cid, with no inherited acceptance evidence.
                for _ in 0..2 {
                    let (mut socket, cid) = raw_info(port).await;
                    assert_ne!(previous, Some(cid));
                    previous = Some(cid);
                    let conns = fetch_all(&http, &base).await.unwrap();
                    let c = conns.iter().find(|c| c.cid == cid).unwrap();
                    assert!(c.name.is_none());
                    assert!(c.authorized_user.is_none());
                    assert_eq!(
                        (c.in_msgs, c.out_msgs, c.subscriptions),
                        (Some(0), Some(0), Some(0))
                    );
                    assert_eq!(
                        classify_connection(c, &probe_holding(None), Evidence::Unproven),
                        LABEL_PENDING
                    );
                    // No credentials: metadata is present, but authentication fails.
                    socket
                        .get_mut()
                        .write_all(
                            b"CONNECT {\"verbose\":false,\"lang\":\"test\",\"version\":\"1\"}\r\n",
                        )
                        .await
                        .unwrap();
                    let mut reply = String::new();
                    socket.read_line(&mut reply).await.unwrap();
                    assert!(reply.contains("Authorization Violation"), "{reply}");
                    let mut removed = false;
                    for _ in 0..50 {
                        if !fetch_all(&http, &base)
                            .await
                            .unwrap()
                            .iter()
                            .any(|c| c.cid == cid)
                        {
                            removed = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(removed, "rejected cid remained in monitoring");
                }
                let opts = if users {
                    async_nats::ConnectOptions::new()
                        .user_and_password("agent".into(), "test-password".into())
                } else {
                    async_nats::ConnectOptions::new().token("test-token".into())
                };
                let authenticated = opts
                    .connect(format!("nats://127.0.0.1:{port}"))
                    .await
                    .unwrap();
                authenticated.flush().await.unwrap();
                let conns = fetch_all(&http, &base).await.unwrap();
                let c = conns.iter().find(|c| c.authorized_user.is_some()).unwrap();
                assert!(c.name.as_deref().is_none_or(str::is_empty));
                let expected = if users { "agent" } else { LABEL_SHARED_TOKEN };
                let ev = if users {
                    Evidence::UsersMode
                } else {
                    Evidence::TokenMode
                };
                assert_eq!(
                    classify_connection(c, &probe_holding(Some("test-token")), ev),
                    expected
                );
                drop(authenticated);
                server.kill().await.unwrap();
                server.wait().await.unwrap();
            }
        })
        .await
        .expect("live auth test deadline");
    }

    #[tokio::test]
    #[ignore = "requires disposable nats-server and loopback sockets"]
    async fn live_connz_open_broker_has_positive_acceptance_evidence() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(15), async {
            let (mut server, _dir, port, base) = disposable_broker("").await;
            let (mut socket, cid) = raw_info(port).await;
            socket.get_mut().write_all(b"CONNECT {\"verbose\":false}\r\nSUB audit.test 1\r\nPUB audit.test 1\r\nx\r\nPING\r\n").await.unwrap();
            loop {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                if line == "PONG\r\n" { break; }
            }
            let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
            let conns = fetch_all(&http, &base).await.unwrap();
            let c = conns.iter().find(|c| c.cid == cid).unwrap();
            assert_eq!(classify_connection(c, &probe_holding(None), Evidence::Unproven), LABEL_NO_AUTH);
            let findings = super::super::nats_auth_findings::group(
                kanade_shared::wire::NatsAuthMode::Token,
                &observe(&conns, &probe_holding(None), Evidence::Unproven),
                &HashSet::new(),
            );
            assert!(findings.iter().any(|f| f.kind == super::super::nats_auth_findings::FindingKind::BrokerOpen));
            server.kill().await.unwrap();
            server.wait().await.unwrap();
        }).await.expect("live open-broker test deadline");
    }

    #[tokio::test]
    #[ignore = "requires disposable nats-server and loopback sockets"]
    async fn live_connz_websocket_token_override_is_not_an_open_broker() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let ws_port = portpicker::pick_unused_port().unwrap();
            let config = format!("websocket {{ port: {ws_port}, no_tls: true, authorization {{ token: ws-test-token }} }}");
            let (mut server, _dir, _port, base) = disposable_broker(&config).await;
            let authenticated = async_nats::ConnectOptions::new().token("ws-test-token".into())
                .connect(format!("ws://127.0.0.1:{ws_port}")).await.unwrap();
            authenticated.flush().await.unwrap();
            let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
            let conns = fetch_all(&http, &base).await.unwrap();
            let c = conns.iter().find(|c| c.transport.as_deref() == Some("websocket")).unwrap();
            assert_eq!(classify_connection(c, &probe_holding(None), Evidence::UsersMode), LABEL_SHARED_TOKEN);
            drop(authenticated);
            server.kill().await.unwrap();
            server.wait().await.unwrap();
        }).await.expect("live websocket test deadline");
    }
    async fn wss_proxy(
        ws_port: u16,
        dir: &std::path::Path,
        cookie: Option<&str>,
    ) -> (tokio::process::Child, std::path::PathBuf, String) {
        use tokio::io::AsyncBufReadExt;
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let cert_config = dir.join("cert.cnf");
        std::fs::write(&cert_config,
            "[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ext\n[dn]\nCN=localhost\n[ext]\nsubjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n"
        ).unwrap();
        assert!(
            tokio::process::Command::new("openssl")
                .args([
                    "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-config"
                ])
                .arg(&cert_config)
                .arg("-keyout")
                .arg(&key)
                .arg("-out")
                .arg(&cert)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .unwrap()
                .success()
        );
        let mut command = tokio::process::Command::new("python3");
        command.env_remove("KANADE_TEST_WS_COOKIE");
        if let Some(cookie) = cookie {
            command.env("KANADE_TEST_WS_COOKIE", cookie);
        }
        let mut proxy = command
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/ci/nats-wss-proxy.py"
            ))
            .arg(ws_port.to_string())
            .arg(&cert)
            .arg(&key)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = tokio::io::BufReader::new(proxy.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).await.unwrap();
        let proxy_port: u16 = line.trim().parse().unwrap();
        let url = format!("wss://127.0.0.1:{proxy_port}");
        (proxy, cert, url)
    }

    #[test]
    fn identity_contract_rejects_unsupported_stale_and_mismatched_bindings() {
        let now = Utc::now();
        let mut broker = snapshot_fixture();
        broker.now = now;
        broker.auth_identity_version = Some(1);
        broker.auth_identity_generation = Some(7);
        let mut c = connection_fixture();
        let fixture = serde_json::json!({"version":1,"server_id":broker.server_id,
            "server_start":broker.start,"cid":c.cid,"connection_start":c.start,
            "transport":"nats","generation":7,"sequence":2,"state":"accepted",
            "mechanism":"user_password","source":"connect","public_user":"agent"});
        for field in [
            "version",
            "server_id",
            "server_start",
            "cid",
            "connection_start",
            "transport",
            "generation",
            "sequence",
            "state",
            "mechanism",
            "public_user",
        ] {
            let mut broken = fixture.clone();
            broken[field] = match field {
                "version" | "cid" | "generation" | "sequence" => 0.into(),
                "server_start" | "connection_start" => serde_json::to_value(now).unwrap(),
                "public_user" => "credential canary".into(),
                _ => "unsupported".into(),
            };
            c.auth_identity = Some(serde_json::from_value(broken).unwrap());
            assert_eq!(
                verified_identity(&c, &broker, now).as_deref(),
                Some(LABEL_UNKNOWN)
            );
        }
        c.auth_identity = Some(serde_json::from_value(fixture).unwrap());
        assert_eq!(
            verified_identity(&c, &broker, now).as_deref(),
            Some("agent")
        );
        assert_eq!(
            verified_identity(&c, &broker, now + chrono::Duration::minutes(2)).as_deref(),
            Some(LABEL_UNKNOWN)
        );
        broker.auth_identity_version = Some(2);
        assert_eq!(
            verified_identity(&c, &broker, now).as_deref(),
            Some(LABEL_UNKNOWN)
        );
        broker.auth_identity_version = None;
        assert_eq!(
            verified_identity(&c, &broker, now).as_deref(),
            Some(LABEL_UNKNOWN)
        );
        c.auth_identity = None;
        assert!(verified_identity(&c, &broker, now).is_none());
    }

    #[tokio::test]
    #[ignore = "requires the reproducibly built identity producer, Python and OpenSSL"]
    async fn live_identity_producer_wss_named_spoof_cookie_and_anonymous() {
        tokio::time::timeout(Duration::from_secs(60), async {
            for case in ["user", "spoof", "cookie", "no_auth_user", "token", "anonymous"] {
                let ws_port = portpicker::pick_unused_port().unwrap();
                let users = if matches!(case, "token" | "anonymous") { "" } else {
                    "authorization { users: [{user: agent, password: fixture-password}, {user: backend, password: other-password}] }"
                };
                let ws_auth = match case {
                    "cookie" => "user_cookie: fixture_user, pass_cookie: fixture_pass",
                    "no_auth_user" => "no_auth_user: agent",
                    "token" => "authorization { token: fixture-token }",
                    _ => "",
                };
                let config = format!("{users}\nwebsocket {{ port: {ws_port}, no_tls: true, {ws_auth} }}");
                let (mut server, dir, port, base) = disposable_broker(&config).await;
                let cookie = (case == "cookie").then_some("fixture_user=agent; fixture_pass=fixture-password");
                let (mut proxy, cert, url) = wss_proxy(ws_port, dir.path(), cookie).await;
                let opts = match case {
                    "user" => async_nats::ConnectOptions::new().user_and_password("agent".into(), "fixture-password".into()),
                    "spoof" => async_nats::ConnectOptions::with_auth_callback(|_| async {
                        let mut auth = async_nats::Auth::new();
                        auth.username = Some("agent".into()); auth.password = Some("fixture-password".into());
                        auth.nkey = Some("broker-claim-canary".into()); Ok(auth)
                    }),
                    "token" => async_nats::ConnectOptions::new().token("fixture-token".into()),
                    _ => async_nats::ConnectOptions::new(),
                };
                let client = opts.name("kanade-agent/PC001").add_root_certificates(cert)
                    .connect(&url).await.unwrap();
                client.flush().await.unwrap();
                let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
                let (conns, _, broker) = fetch_walk(&http, &base).await.unwrap();
                assert_eq!(broker.auth_identity_version, Some(1), "a real producer is mandatory");
                let c = conns.iter().find(|c| c.cid == client.server_info().client_id).unwrap();
                let expected = match case { "token" => LABEL_SHARED_TOKEN,
                    "no_auth_user" | "anonymous" => LABEL_NO_AUTH, _ => "agent" };
                assert_eq!(classify_connection(c, &probe_holding(None), Evidence::Unproven), expected);
                if case == "spoof" { assert_eq!(c.authorized_user.as_deref(), Some("broker-claim-canary")); }
                let pool = pool_with(&["PC001"]).await;
                apply_identity_projection(&pool, &conns, &probe_holding(None), Evidence::Unproven).await.unwrap();
                assert_eq!(read(&pool, "PC001").await.0.as_deref(), Some(expected));
                if case == "user" {
                    let tcp = async_nats::ConnectOptions::new().user_and_password("agent".into(), "fixture-password".into())
                        .connect(format!("nats://127.0.0.1:{port}")).await.unwrap(); tcp.flush().await.unwrap();
                    let conns = fetch_all(&http, &base).await.unwrap();
                    let tcp_c = conns.iter().find(|c| c.cid == tcp.server_info().client_id).unwrap();
                    assert_eq!(classify_connection(tcp_c, &probe_holding(None), Evidence::Unproven), "agent");
                    drop(tcp);
                }
                drop(client); proxy.kill().await.unwrap();proxy.wait().await.unwrap();
                server.kill().await.unwrap();server.wait().await.unwrap();
            }
        }).await.expect("real producer WSS deadline");
    }

    #[tokio::test]
    #[ignore = "requires the reproducibly built identity producer, Python and OpenSSL"]
    async fn live_identity_producer_named_expiry_reload_restart_rollback_and_ui() {
        tokio::time::timeout(Duration::from_secs(120), async {
            let ws_port = portpicker::pick_unused_port().unwrap();
            let config = format!("authorization {{ users: [{{user: agent, password: fixture-password}}] }}\nwebsocket {{ port: {ws_port}, no_tls: true }}");
            let (mut server, dir, _port, base) = disposable_broker(&config).await;
            let (mut proxy, cert, url) = wss_proxy(ws_port, dir.path(), None).await;
            let opts = || async_nats::ConnectOptions::new().user_and_password("agent".into(), "fixture-password".into())
                .name("kanade-agent/PC001").add_root_certificates(cert.clone());
            let client = opts().connect(&url).await.unwrap();client.flush().await.unwrap();
            let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
            let (mut conns, complete, before) = fetch_walk(&http, &base).await.unwrap();
            assert_eq!(before.auth_identity_version, Some(1));
            let pool = pool_with(&["PC001"]).await;
            apply_identity_projection(&pool, &conns, &probe_holding(None), Evidence::Unproven).await.unwrap();
            assert_eq!(read(&pool, "PC001").await.0.as_deref(), Some("agent"));
            let settings = kanade_shared::wire::ServerSettings {
                nats_auth_mode: Some(kanade_shared::wire::NatsAuthMode::Users), ..Default::default()
            };
            let observed = observe(&conns, &probe_holding(None), Evidence::Unproven);
            nats_auth_audit::run_poll(&pool, Ok(settings.clone()), &nats_auth_audit::Poll {
                observed: &observed, registered: &HashSet::from(["PC001".into()]), complete, uncertainty_reasons: None,
            }, Utc::now()).await.unwrap();
            // This HTTP server calls the production Agents API, against the
            // actual producer-derived projection. The browser uses a real
            // Vite HTTP proxy; no API rows or metadata are stubbed.
            let api = axum::Router::new()
                .route("/api/agents", axum::routing::get(crate::api::agents::list))
                .route("/test/expire", axum::routing::post(|axum::extract::State(pool): axum::extract::State<SqlitePool>| async move {
                    sqlx::query("UPDATE agents SET nats_identity_expires_at = '2000-01-01T00:00:00Z' WHERE nats_identity_expires_at IS NOT NULL").execute(&pool).await.unwrap();
                    axum::http::StatusCode::NO_CONTENT
                }))
                .route("/test/health", axum::routing::get(|axum::extract::State(pool): axum::extract::State<SqlitePool>| async move {
                    axum::Json(nats_auth_audit::health_block(&pool, None, Utc::now()).await)
                })).with_state(pool.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let serving = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
            if std::env::var_os("KANADE_IDENTITY_BROWSER").is_some() {
                let output = tokio::process::Command::new("bun")
                    .args(["run", "test:ct", "src/pages/Agents.identity.ct.tsx"])
                    .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/web"))
                    .env("KANADE_IDENTITY_API", format!("http://{address}"))
                    .output().await.unwrap();
                assert!(output.status.success(), "producer-derived Agents browser test failed");
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "positive browser test did not execute");
            }
            // API expiry works independently of a ticking projector.
            sqlx::query("UPDATE agents SET nats_identity_expires_at = '2000-01-01T00:00:00Z'").execute(&pool).await.unwrap();
            let (_, axum::Json(rows)) = crate::api::agents::list(axum::extract::State(pool.clone()), axum::extract::Query(Default::default()), axum::extract::Query(Vec::new())).await.unwrap();
            assert_eq!(rows[0].nats_user.as_deref(), Some(LABEL_UNKNOWN));
            // Unsupported producer / rollback must revoke a previously named
            // projection, even if the old raw monitoring value looks familiar.
            let mut old: Vec<ConnInfo> = serde_json::from_value(serde_json::json!([{
                "cid":client.server_info().client_id,"kind":"Client","type":"websocket",
                "name":"kanade-agent/PC001","authorized_user":"agent"
            }])).unwrap();
            apply_identity_projection(&pool, &old, &probe_holding(None), Evidence::UsersMode).await.unwrap();
            assert_eq!(read(&pool, "PC001").await.0.as_deref(), Some(LABEL_UNKNOWN));
            apply_identity_projection(&pool, &conns, &probe_holding(None), Evidence::Unproven).await.unwrap();
            assert_eq!(read(&pool, "PC001").await.0.as_deref(), Some("agent"));
            let path = dir.path().join("nats.conf");
            let text = std::fs::read_to_string(&path).unwrap();
            // A non-auth reload still invalidates the cached result. Existing
            // sockets must reconnect or be revalidated before they regain it.
            std::fs::write(&path, format!("{text}\nmax_payload: 1048577\n")).unwrap();
            assert!(tokio::process::Command::new("nats-server").arg("--signal")
                .arg(format!("reload={}",server.id().unwrap())).stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()).status().await.unwrap().success());
            let mut after = None;
            for _ in 0..100 {
                let result = fetch_walk(&http,&base).await;
                if let Ok((c,_,v)) = result && v.auth_identity_generation != before.auth_identity_generation {
                    after=Some((c,v));break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let (reloaded, epoch) = after.expect("reload generation");
            assert!(!before.same_epoch(&epoch));
            // Old proof is unusable after a reload, even if the broker
            // immediately revalidates the live connection into the new epoch.
            for c in &mut conns { c.identity_label = verified_identity(c, &epoch, Utc::now()); }
            apply_identity_projection(&pool, &conns, &probe_holding(None), Evidence::Unproven).await.unwrap();
            assert_eq!(read(&pool,"PC001").await.0.as_deref(),Some(LABEL_UNKNOWN));
            assert!(reloaded.iter().all(|c| c.auth_identity.as_ref().is_none_or(|a| a.generation != before.auth_identity_generation.unwrap() || c.identity_label.as_deref() != Some("agent"))));
            drop(client);
            let new_client=opts().connect(&url).await.unwrap();new_client.flush().await.unwrap();
            let fresh=fetch_all(&http,&base).await.unwrap();
            apply_identity_projection(&pool,&fresh,&probe_holding(None),Evidence::Unproven).await.unwrap();
            assert_eq!(read(&pool,"PC001").await.0.as_deref(),Some("agent"));
            server.kill().await.unwrap();server.wait().await.unwrap();
            invalidate_identity_proofs(&pool).await.unwrap();
            assert_eq!(read(&pool,"PC001").await.0.as_deref(),Some(LABEL_UNKNOWN));
            server=tokio::process::Command::new("nats-server").arg("-c").arg(&path)
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true).spawn().unwrap();
            let mut restart=None;
            for _ in 0..100 { if let Ok(v)=fetch_json::<Varz>(&http,&format!("{base}/varz")).await {restart=Some(v);break;}
                tokio::time::sleep(Duration::from_millis(20)).await; }
            let restart=restart.unwrap();assert_ne!(restart.server_id,before.server_id);
            let previous=conns.iter().find(|c|c.auth_identity.is_some()).unwrap();
            assert_eq!(verified_identity(previous,&restart,Utc::now()).as_deref(),Some(LABEL_UNKNOWN));
            old.clear();
            let health=nats_auth_audit::health_block(&pool,Some(&settings),Utc::now()).await;
            let history=super::super::nats_auth_evidence::history(&pool,None).await.unwrap();
            let safe=serde_json::to_string(&(health,history)).unwrap();
            for value in ["fixture-password","127.0.0.1","broker-claim-canary"] {assert!(!safe.contains(value));}
            serving.abort();let _=serving.await;
            drop(new_client);proxy.kill().await.unwrap();proxy.wait().await.unwrap();
            server.kill().await.unwrap();server.wait().await.unwrap();
        }).await.expect("producer lifecycle and browser deadline");
    }

    #[tokio::test]
    #[ignore = "requires disposable nats-server, Python, OpenSSL and loopback sockets"]
    async fn live_connz_wss_users_acceptance_does_not_prove_reported_identity() {
        tokio::time::timeout(Duration::from_secs(40), async {
            let ws_port = portpicker::pick_unused_port().unwrap();
            let config = format!(
                "authorization {{ users: [{{user: agent, password: test-password}}, {{user: backend, password: other-password}}] }}\nwebsocket {{ port: {ws_port}, no_tls: true }}"
            );
            let (mut server, dir, port, base) = disposable_broker(&config).await;
            let (mut proxy, cert, url) = wss_proxy(ws_port, dir.path(), None).await;
            let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
            // The disposable certificate must not work without explicit trust.
            assert!(async_nats::ConnectOptions::new()
                .user_and_password("agent".into(), "test-password".into())
                .connect(&url).await.is_err());
            // These are actual broker authentication errors through the TLS
            // proxy, not HTTP errors or success inferred from TCP policy.
            for opts in [
                async_nats::ConnectOptions::new(),
                async_nats::ConnectOptions::new().user_and_password("agent".into(), "wrong-password".into()),
                async_nats::ConnectOptions::new().token("test-token".into()),
            ] {
                let err = opts.add_root_certificates(cert.clone()).connect(&url).await.unwrap_err();
                assert_eq!(err.kind(), async_nats::ConnectErrorKind::AuthorizationViolation);
            }
            let tcp = async_nats::ConnectOptions::new()
                .user_and_password("agent".into(), "test-password".into())
                .connect(format!("nats://127.0.0.1:{port}")).await.unwrap();
            tcp.flush().await.unwrap();
            let pool = pool_with(&["PC001"]).await;
            // Even a configured username allowlist would accept "backend".
            // Yet this client authenticates with the agent password: the
            // ignored nkey CONNECT field controls getRawAuthUser/connz.
            for reported in [None, Some("backend"), Some("credential-canary-not-a-username")] {
                let injected = reported.map(str::to_string);
                let ws = async_nats::ConnectOptions::with_auth_callback(move |_| {
                    let injected = injected.clone();
                    async move {
                        let mut auth = async_nats::Auth::new();
                        auth.username = Some("agent".into());
                        auth.password = Some("test-password".into());
                        auth.nkey = injected;
                        Ok(auth)
                    }
                })
                .name("kanade-agent/PC001").add_root_certificates(cert.clone())
                .connect(&url).await.unwrap();
                ws.flush().await.unwrap();
                let cid = ws.server_info().client_id;
                let conns = fetch_all(&http, &base).await.unwrap();
                let c = conns.iter().find(|c| c.cid == cid).unwrap();
                assert_eq!(c.transport.as_deref(), Some("websocket"));
                assert_eq!(c.authorized_user.as_deref(), Some(reported.unwrap_or("agent")));
                assert_eq!(classify_connection(c, &probe_holding(None), Evidence::UsersMode), LABEL_UNKNOWN);
                let tcp_conn = conns.iter().find(|c| c.cid == tcp.server_info().client_id).unwrap();
                assert_eq!(classify_connection(tcp_conn, &probe_holding(None), Evidence::UsersMode), "agent");
                let (labels, _) = correlate(&conns, &probe_holding(None), Evidence::UsersMode);
                apply(&pool, &labels).await.unwrap();
                assert_eq!(read(&pool, "PC001").await.0.as_deref(), Some(LABEL_UNKNOWN));
                let settings = kanade_shared::wire::ServerSettings {
                    nats_auth_mode: Some(kanade_shared::wire::NatsAuthMode::Users),
                    ..Default::default()
                };
                let observed = observe(&conns, &probe_holding(None), Evidence::UsersMode);
                nats_auth_audit::run_poll(&pool, Ok(settings.clone()), &nats_auth_audit::Poll {
                    observed: &observed, registered: &HashSet::from(["PC001".into()]),
                    complete: true, uncertainty_reasons: None,
                }, Utc::now()).await.unwrap();
                let health = nats_auth_audit::health_block(&pool, Some(&settings), Utc::now()).await;
                assert!(health.findings.iter().any(|f| f.kind == "credential_unnameable"));
                let history = super::super::nats_auth_evidence::history(&pool, None).await.unwrap();
                let safe = serde_json::to_string(&(health, history)).unwrap();
                for secret in ["credential-canary-not-a-username", "test-password", "other-password", "127.0.0.1"] {
                    assert!(!safe.contains(secret));
                }
                drop(ws);
            }
            drop(tcp);
            proxy.kill().await.unwrap();
            proxy.wait().await.unwrap();
            server.kill().await.unwrap();
            server.wait().await.unwrap();
        }).await.expect("live WSS identity test deadline");
    }

    #[tokio::test]
    #[ignore = "requires disposable nats-server, Python, OpenSSL and loopback sockets"]
    async fn live_connz_wss_anonymous_and_cookie_overrides_do_not_prove_a_name() {
        tokio::time::timeout(Duration::from_secs(40), async {
            let users = "authorization { users: [{user: guest, password: cookie-password}] }";
            for (auth, ws_auth, cookie, expected) in [
                (users, "no_auth_user: guest", None, LABEL_UNKNOWN),
                (
                    users,
                    "user_cookie: fixture_user, pass_cookie: fixture_pass",
                    Some("fixture_user=guest; fixture_pass=cookie-password"),
                    LABEL_UNKNOWN,
                ),
                (
                    "",
                    "authorization { token: cookie-token }, token_cookie: fixture_token",
                    Some("fixture_token=cookie-token"),
                    LABEL_SHARED_TOKEN,
                ),
            ] {
                let ws_port = portpicker::pick_unused_port().unwrap();
                let config =
                    format!("{auth}\nwebsocket {{ port: {ws_port}, no_tls: true, {ws_auth} }}");
                let (mut server, dir, _port, base) = disposable_broker(&config).await;
                if cookie.is_some() {
                    // Without the cookie, the same TLS/WebSocket listener
                    // must reject CONNECT with a broker authorization error.
                    let (mut proxy, cert, url) = wss_proxy(ws_port, dir.path(), None).await;
                    let err = async_nats::ConnectOptions::new()
                        .add_root_certificates(cert)
                        .connect(&url)
                        .await
                        .unwrap_err();
                    assert_eq!(
                        err.kind(),
                        async_nats::ConnectErrorKind::AuthorizationViolation
                    );
                    proxy.kill().await.unwrap();
                    proxy.wait().await.unwrap();
                }
                let (mut proxy, cert, url) = wss_proxy(ws_port, dir.path(), cookie).await;
                let client = async_nats::ConnectOptions::new()
                    .name("kanade-agent/PC001")
                    .add_root_certificates(cert)
                    .connect(&url)
                    .await
                    .unwrap();
                let sub = client.subscribe("audit.test").await.unwrap();
                client.flush().await.unwrap();
                let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
                let (conns, complete, _) = fetch_walk(&http, &base).await.unwrap();
                let c = conns
                    .iter()
                    .find(|c| c.cid == client.server_info().client_id)
                    .unwrap();
                assert_eq!(c.transport.as_deref(), Some("websocket"));
                assert!(c.subscriptions.is_some_and(|n| n > 0));
                assert_eq!(
                    classify_connection(c, &probe_holding(None), Evidence::UsersMode),
                    expected
                );
                let pool = pool_with(&["PC001"]).await;
                let (labels, _) = correlate(&conns, &probe_holding(None), Evidence::UsersMode);
                apply(&pool, &labels).await.unwrap();
                assert_eq!(read(&pool, "PC001").await.0.as_deref(), Some(expected));
                let settings = kanade_shared::wire::ServerSettings {
                    nats_auth_mode: Some(kanade_shared::wire::NatsAuthMode::Users),
                    ..Default::default()
                };
                let observed = observe(&conns, &probe_holding(None), Evidence::UsersMode);
                nats_auth_audit::run_poll(
                    &pool,
                    Ok(settings.clone()),
                    &nats_auth_audit::Poll {
                        observed: &observed,
                        registered: &HashSet::from(["PC001".into()]),
                        complete,
                        uncertainty_reasons: None,
                    },
                    Utc::now(),
                )
                .await
                .unwrap();
                let health =
                    nats_auth_audit::health_block(&pool, Some(&settings), Utc::now()).await;
                let history = super::super::nats_auth_evidence::history(&pool, None)
                    .await
                    .unwrap();
                let safe = serde_json::to_string(&(health, history)).unwrap();
                for forbidden in [
                    "cookie-password",
                    "cookie-token",
                    "fixture_user",
                    "fixture_pass",
                    "fixture_token",
                    "127.0.0.1",
                ] {
                    assert!(!safe.contains(forbidden));
                }
                drop(sub);
                drop(client);
                proxy.kill().await.unwrap();
                proxy.wait().await.unwrap();
                if let Some(cookie) = cookie {
                    let invalid = cookie
                        .replace("cookie-password", "wrong-password")
                        .replace("cookie-token", "wrong-token");
                    let (mut bad_proxy, cert, url) =
                        wss_proxy(ws_port, dir.path(), Some(&invalid)).await;
                    let err = async_nats::ConnectOptions::new()
                        .add_root_certificates(cert)
                        .connect(&url)
                        .await
                        .unwrap_err();
                    assert_eq!(
                        err.kind(),
                        async_nats::ConnectErrorKind::AuthorizationViolation
                    );
                    bad_proxy.kill().await.unwrap();
                    bad_proxy.wait().await.unwrap();
                }
                server.kill().await.unwrap();
                server.wait().await.unwrap();
            }
        })
        .await
        .expect("live WSS override deadline");
    }

    #[tokio::test]
    #[ignore = "requires disposable nats-server, Python, OpenSSL and loopback sockets"]
    async fn live_connz_wss_reload_restart_and_expiry_do_not_reuse_identity_evidence() {
        tokio::time::timeout(Duration::from_secs(40), async {
            let ws_port = portpicker::pick_unused_port().unwrap();
            let config = format!("authorization {{ users: [{{user: agent, password: first-password}}] }}\nwebsocket {{ port: {ws_port}, no_tls: true }}");
            let (mut server, dir, _port, base) = disposable_broker(&config).await;
            let (mut proxy, cert, url) = wss_proxy(ws_port, dir.path(), None).await;
            let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
            let opts = |password: &str| async_nats::ConnectOptions::new()
                .user_and_password("agent".into(), password.into())
                .name("kanade-agent/PC001").max_reconnects(1)
                .add_root_certificates(cert.clone());
            let first = opts("first-password").connect(&url).await.unwrap();
            first.flush().await.unwrap();
            let first_cid = first.server_info().client_id;
            let (_, _, before) = fetch_walk(&http, &base).await.unwrap();
            let config_path = dir.path().join("nats.conf");
            let text = std::fs::read_to_string(&config_path).unwrap();
            std::fs::write(&config_path, text.replace("first-password", "second-password")).unwrap();
            assert!(tokio::process::Command::new("nats-server")
                .arg("--signal").arg(format!("reload={}", server.id().unwrap()))
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                .status().await.unwrap().success());
            let mut reloaded = None;
            for _ in 0..100 {
                let after: Varz = fetch_json(&http, &format!("{base}/varz")).await.unwrap();
                if after.config_load_time > before.config_load_time {
                    reloaded = Some(after);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let after = reloaded.expect("broker reload observed");
            assert!(!before.same_epoch(&after));
            assert_eq!(before.server_id, after.server_id);
            let err = opts("first-password").connect(&url).await.unwrap_err();
            assert_eq!(err.kind(), async_nats::ConnectErrorKind::AuthorizationViolation);
            let second = opts("second-password").connect(&url).await.unwrap();
            second.flush().await.unwrap();
            let second_cid = second.server_info().client_id;
            assert_ne!(first_cid, second_cid);
            let (conns, complete, _) = fetch_walk(&http, &base).await.unwrap();
            assert!(!conns.iter().any(|c| c.cid == first_cid));
            let current = conns.iter().find(|c| c.cid == second_cid).unwrap();
            assert_eq!(classify_connection(current, &probe_holding(None), Evidence::UsersMode), LABEL_UNKNOWN);
            let pool = pool_with(&["PC001"]).await;
            let settings = kanade_shared::wire::ServerSettings {
                nats_auth_mode: Some(kanade_shared::wire::NatsAuthMode::Users),
                ..Default::default()
            };
            let observed = observe(&conns, &probe_holding(None), Evidence::UsersMode);
            let now = Utc::now();
            nats_auth_audit::run_poll(&pool, Ok(settings.clone()), &nats_auth_audit::Poll {
                observed: &observed, registered: &HashSet::from(["PC001".into()]),
                complete, uncertainty_reasons: None,
            }, now).await.unwrap();
            let stale = nats_auth_audit::health_block(&pool, Some(&settings), now + chrono::Duration::minutes(4)).await;
            assert_eq!(stale.poll_status, "stale");
            assert!(stale.uncertainty_reasons.is_none());
            drop(first);
            drop(second);
            server.kill().await.unwrap();
            server.wait().await.unwrap();
            server = tokio::process::Command::new("nats-server").arg("-c").arg(&config_path)
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                .kill_on_drop(true).spawn().unwrap();
            let mut restarted = None;
            for _ in 0..100 {
                if let Ok(v) = fetch_json::<Varz>(&http, &format!("{base}/varz")).await {
                    restarted = Some(v);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let restart = restarted.expect("restarted broker ready");
            assert_ne!(after.server_id, restart.server_id);
            assert_ne!(after.start, restart.start);
            assert!(!after.same_epoch(&restart));
            let third = opts("second-password").connect(&url).await.unwrap();
            third.flush().await.unwrap();
            let conns = fetch_all(&http, &base).await.unwrap();
            let current = conns.iter().find(|c| c.cid == third.server_info().client_id).unwrap();
            // A CID may be reused after restart. A stale server id cannot
            // establish evidence even if the numeric id happens to match.
            assert_eq!(monitor_evidence(&CredentialProbe::from_user("agent"), true,
                &after.server_id, current.cid, &restart, &conns), Evidence::Unproven);
            assert_eq!(classify_connection(current, &probe_holding(None), Evidence::UsersMode), LABEL_UNKNOWN);
            let (labels, _) = correlate(&conns, &probe_holding(None), Evidence::UsersMode);
            apply(&pool, &labels).await.unwrap();
            assert_eq!(read(&pool, "PC001").await.0.as_deref(), Some(LABEL_UNKNOWN));
            let history = super::super::nats_auth_evidence::history(&pool, None).await.unwrap();
            let safe = serde_json::to_string(&(stale, history)).unwrap();
            for forbidden in ["first-password", "second-password", "127.0.0.1"] {
                assert!(!safe.contains(forbidden));
            }
            drop(third);
            proxy.kill().await.unwrap();
            proxy.wait().await.unwrap();
            server.kill().await.unwrap();
            server.wait().await.unwrap();
        }).await.expect("live WSS lifecycle deadline");
    }

    #[tokio::test]
    #[ignore = "requires disposable nats-server and loopback sockets"]
    async fn live_connz_websocket_no_auth_user_has_a_broker_assigned_identity() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let ws_port = portpicker::pick_unused_port().unwrap();
            let config = format!("authorization {{ users: [{{user: guest, password: test-password}}] }}\nwebsocket {{ port: {ws_port}, no_tls: true, no_auth_user: guest }}");
            let (mut server, _dir, _port, base) = disposable_broker(&config).await;
            let anonymous = async_nats::ConnectOptions::new()
                .connect(format!("ws://127.0.0.1:{ws_port}")).await.unwrap();
            let sub = anonymous.subscribe("audit.test").await.unwrap();
            anonymous.flush().await.unwrap();
            let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
            let conns = fetch_all(&http, &base).await.unwrap();
            let c = conns.iter().find(|c| c.transport.as_deref() == Some("websocket")).unwrap();
            assert_eq!(c.authorized_user.as_deref(), Some("guest"));
            assert!(c.subscriptions.is_some_and(|n| n > 0));
            // Activity does not override an identity supplied by the broker.
            assert_eq!(classify_connection(c, &probe_holding(None), Evidence::UsersMode), LABEL_UNKNOWN);
            drop(sub);
            drop(anonymous);
            server.kill().await.unwrap();
            server.wait().await.unwrap();
        }).await.expect("live websocket no_auth_user test deadline");
    }
    #[test]
    fn startup_reconnect_and_another_broker_cannot_prove_safe_user_labels() {
        let mut v = snapshot_fixture();
        let mut c = connection_fixture();
        c.authorized_user = Some("backend".into());
        let probe = CredentialProbe::from_user("backend");
        let ev = |connected, id: &str, cid, v: &Varz, c: &ConnInfo| {
            monitor_evidence(&probe, connected, id, cid, v, std::slice::from_ref(c))
        };
        assert_eq!(ev(true, "TESTSERVER", 1, &v, &c), Evidence::UsersMode);
        assert_eq!(ev(false, "TESTSERVER", 1, &v, &c), Evidence::Unproven);
        assert_eq!(ev(true, "another-server", 1, &v, &c), Evidence::Unproven);
        assert_eq!(ev(true, "TESTSERVER", 2, &v, &c), Evidence::Unproven);
        v.auth_required = None;
        assert_eq!(ev(true, "TESTSERVER", 1, &v, &c), Evidence::Unproven);
        v.auth_required = Some(true);
        c.authorized_user = None;
        assert_eq!(ev(true, "TESTSERVER", 1, &v, &c), Evidence::Unproven);
        c.authorized_user = Some("backend".into());
        c.transport = Some("websocket".into());
        assert_eq!(ev(true, "TESTSERVER", 1, &v, &c), Evidence::Unproven);
    }
    #[test]
    fn old_traffic_during_reauthentication_cannot_override_listener_auth() {
        let mut v = snapshot_fixture();
        let probe = probe_holding(None);
        for transport in ["nats", "websocket"] {
            for policy in [Some(true), Some(false), None] {
                if transport == "nats" && policy != Some(true) {
                    continue;
                }
                v.auth_required = policy;
                for name in [None, Some("kanade-cli")] {
                    // Snapshot after an authenticated CONNECT/PUB, while a
                    // second CONNECT with an empty token has overwritten opts
                    // but has not yet failed authentication. WS may use its
                    // own token even when TCP auth_required is false/absent.
                    let mut c = connection_fixture();
                    c.transport = Some(transport.into());
                    c.in_msgs = Some(12);
                    c.out_msgs = Some(3);
                    c.subscriptions = Some(2);
                    c.name = name.map(str::to_owned);
                    c.authorized_user = Some(String::new());
                    c.acceptance = v.acceptance(&c, v.now);
                    assert_eq!(c.acceptance.label(), LABEL_UNPROVEN);
                    let reason = if transport == "websocket" {
                        EvidenceReason::ListenerAuthUnproven
                    } else {
                        EvidenceReason::AuthPolicyConflict
                    };
                    assert_eq!(
                        uncertainty_reasons(std::slice::from_ref(&c)),
                        BTreeMap::from([(reason, 1)])
                    );
                    let observations = observe(&[c], &probe, Evidence::Unproven);
                    let findings = super::super::nats_auth_findings::group(
                        kanade_shared::wire::NatsAuthMode::Token,
                        &observations,
                        &HashSet::new(),
                    );
                    assert!(findings.is_empty(), "{transport} {policy:?} {name:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn overlapping_cid_boundaries_tolerate_appends_but_detect_skipped_rows() {
        let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let page = |offset: usize, total: usize, cids: &[u64]| {
            serde_json::json!({
                "server_id":"TESTSERVER","now":"2026-10-18T02:24:58Z",
                "offset":offset,"num_connections":cids.len(),"total":total,
                "connections":cids.iter().map(|cid| serde_json::json!({"cid":cid})).collect::<Vec<_>>()
            }).to_string()
        };
        let mut after: serde_json::Value = serde_json::from_str(&varz_fixture()).unwrap();
        after["total_connections"] = 100.into();
        for (next, expected_complete, expected_ids) in [
            // New higher CIDs append without skipping any old connections.
            (page(1, 4, &[2, 3, 4]), true, vec![1, 2, 3, 4]),
            // A connection after the boundary can close before it is read.
            (page(1, 2, &[2]), true, vec![1, 2]),
            // CID 1 disappeared before offset 1: CID 3 would be skipped by
            // an ordinary non-overlapping walk. Preserve positives but hold
            // absence claims when the expected boundary CID 2 is missing.
            (page(1, 3, &[3, 4]), false, vec![1, 2, 3, 4]),
            // Re-reading the anchor without making progress terminates.
            (page(1, 3, &[2]), false, vec![1, 2]),
        ] {
            let (base, queries) = canned_http(vec![
                varz_fixture(),
                page(0, 3, &[1, 2]),
                next,
                after.to_string(),
            ])
            .await;
            let (conns, complete, _) = fetch_walk(&http, &base).await.unwrap();
            assert_eq!(complete, expected_complete);
            assert_eq!(
                conns.iter().map(|c| c.cid).collect::<Vec<_>>(),
                expected_ids
            );
            let queries = queries.lock().unwrap();
            assert!(queries[1].contains("sort=cid"));
            assert!(queries[2].contains("offset=1"));
        }
    }

    #[tokio::test]
    async fn page_identity_changes_and_duplicate_cids_cannot_resolve_findings() {
        let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let first = serde_json::json!({
            "server_id":"TESTSERVER","now":"2026-10-18T02:24:58Z",
            "offset":0,"num_connections":1,"total":2,"connections":[{"cid":1}]
        });
        let mut next = first.clone();
        next["num_connections"] = 2.into();
        next["connections"] = serde_json::json!([{"cid":1},{"cid":1}]);
        // The anchor is expected once; another copy must not count twice.
        let (base, _) = canned_http(vec![
            varz_fixture(),
            first.to_string(),
            next.to_string(),
            varz_fixture(),
        ])
        .await;
        let (conns, complete, _) = fetch_walk(&http, &base).await.unwrap();
        assert_eq!(conns.len(), 1);
        assert!(!complete);
        next["server_id"] = "another-broker".into();
        next["connections"][0]["cid"] = 2.into();
        let (base, _) =
            canned_http(vec![varz_fixture(), first.to_string(), next.to_string()]).await;
        assert!(fetch_walk(&http, &base).await.is_err());
    }

    #[tokio::test]
    async fn malformed_monitoring_values_cannot_leak_into_errors() {
        let secret = "example-secret-value";
        let (base, _) = canned_http(vec![format!(
            r#"{{"server_id":"test","start":"{secret}"}}"#
        )])
        .await;
        let http: MonitorClient = Client::builder(TokioExecutor::new()).build_http();
        let err = fetch_walk(&http, &base).await.unwrap_err();
        assert!(!format!("{err:#}").contains(secret));
    }
}
