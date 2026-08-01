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

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use kanade_shared::nats_client::{CredentialKind, CredentialProbe, NatsRole, parse_client_name};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

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
/// The broker authenticated nobody: no `authorization` block at all.
pub const LABEL_NO_AUTH: &str = "no-auth";
/// Connected, but the credential cannot be named without disclosing a
/// secret. Distinct from NULL, which means "not seen at all".
pub const LABEL_UNKNOWN: &str = "unknown";

/// The subset of `/connz` this projector reads.
#[derive(Debug, Deserialize)]
struct Connz {
    /// Connections matching the query across ALL pages — the paging bound.
    #[serde(default)]
    total: usize,
    #[serde(default)]
    connections: Vec<ConnInfo>,
}

#[derive(Debug, Deserialize)]
struct ConnInfo {
    /// Server-assigned connection id. Used only to break ties between two
    /// live connections claiming one pc_id: higher = more recent.
    #[serde(default)]
    cid: u64,
    /// The client-supplied connection name. Absent for clients that set
    /// none (the `nats` CLI does set one; a raw library user may not).
    #[serde(default)]
    name: Option<String>,
    /// Present only with `auth=1`. Empty / absent when the broker required
    /// no credential.
    #[serde(default)]
    authorized_user: Option<String>,
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

/// Classify one connection's reported `authorized_user` into a value that
/// is safe to store. See the module docs for why this is not the identity
/// function.
fn classify(authorized_user: Option<&str>, probe: &CredentialProbe, ev: Evidence) -> String {
    match authorized_user.map(str::trim).filter(|s| !s.is_empty()) {
        None => LABEL_NO_AUTH.to_string(),
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
        let label = classify(c.authorized_user.as_deref(), probe, ev);
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
    let url = format!("{base}/connz?auth=1&subs=0&limit={PAGE_SIZE}&offset={offset}");
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
    serde_json::from_slice(&body).with_context(|| format!("decode /connz from {url}"))
}

/// Walk every page of `/connz` and return the connections.
async fn fetch_all(client: &MonitorClient, base: &str) -> Result<Vec<ConnInfo>> {
    let mut out: Vec<ConnInfo> = Vec::new();
    for page in 0..MAX_PAGES {
        let z = fetch_page(client, base, out.len()).await?;
        let got = z.connections.len();
        out.extend(z.connections);
        // Stop on a short page (the last one) or once the broker's own
        // total is covered. A page that returns nothing also stops us —
        // otherwise a broker whose `total` outruns what it will serve would
        // loop until MAX_PAGES every poll.
        if got == 0 || out.len() >= z.total {
            return Ok(out);
        }
        if page + 1 == MAX_PAGES {
            warn!(
                pages = MAX_PAGES,
                fetched = out.len(),
                total = z.total,
                "/connz paging hit its cap; the tail of the connection list was not read",
            );
        }
    }
    Ok(out)
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
/// `nats` is the backend's own broker connection, and is read for one thing:
/// whether it is currently up. That is what turns a locally-resolved
/// credential into evidence about the broker — see [`Evidence`].
pub async fn run(pool: SqlitePool, monitor_url: String, nats: async_nats::Client) -> Result<()> {
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
        // Dropping the `fetch_all` future on timeout cancels the in-flight
        // request and closes the connection, so a wedged endpoint costs one
        // poll rather than the projector.
        let polled = match tokio::time::timeout(POLL_TIMEOUT, fetch_all(&client, &monitor_url))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "poll timed out after {POLL_TIMEOUT:?} (endpoint accepted the connection but did \
                 not finish answering)"
            )),
        };
        match polled {
            Ok(conns) => {
                let seen = conns.len();
                // Re-read per poll: the backend's link can drop and come
                // back, and the proof is only as current as the connection.
                let ev = evidence(
                    &probe,
                    nats.connection_state() == async_nats::connection::State::Connected,
                );
                let (labels, anomalies) = correlate(&conns, &probe, ev);
                let correlated = labels.len();
                if anomalies != reported {
                    if !anomalies.non_agent.is_empty() {
                        warn!(
                            connections = ?anomalies.non_agent,
                            "NATS connections name a pc_id under a non-agent role; ignored",
                        );
                    }
                    if !anomalies.conflicting.is_empty() {
                        warn!(
                            hosts = ?anomalies.conflicting,
                            "two live NATS connections claim one pc_id with different credentials",
                        );
                    }
                    if anomalies.is_empty() {
                        info!("previously reported NATS connection anomalies have cleared");
                    }
                    reported = anomalies;
                }
                match apply(&pool, &labels).await {
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
        serde_json::from_str(json).expect("decode /connz")
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
    fn no_credential_at_all_is_its_own_state() {
        let probe = probe_holding(None);
        for reported in [None, Some(""), Some("   ")] {
            assert_eq!(
                classify(reported, &probe, Evidence::Unproven),
                LABEL_NO_AUTH,
                "{reported:?} means the broker authenticated nobody",
            );
        }
        // Even holding a token, an unauthenticated connection is not "ours".
        let probe = probe_holding(Some("fleet-secret"));
        assert_eq!(classify(None, &probe, Evidence::TokenMode), LABEL_NO_AUTH);
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
        let (base, queries) = canned_connz(vec![page(&[1, 2]), page(&[3])]).await;
        let client: MonitorClient = Client::builder(TokioExecutor::new()).build_http();

        let conns = fetch_all(&client, &base).await.expect("walk both pages");
        assert_eq!(conns.len(), 3, "the tail of the fleet must not be dropped");

        // The second request must resume where the first stopped, or the
        // walk silently re-reads page one until MAX_PAGES.
        let queries = queries.lock().unwrap().clone();
        assert_eq!(queries.len(), 2, "{queries:?}");
        assert!(queries[0].contains("offset=0"), "{}", queries[0]);
        assert!(queries[1].contains("offset=2"), "{}", queries[1]);
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
            queries.lock().unwrap().len(),
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
        let conns = fetch_all(&http, &base).await.expect("read live /connz");
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
}
