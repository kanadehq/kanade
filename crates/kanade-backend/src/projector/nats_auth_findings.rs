//! What counts as an *unintended* broker authentication, and how a poll's
//! worth of connections folds into a few reportable groups.
//!
//! [`nats_conns`](super::nats_conns) already records, per live connection,
//! which credential the **broker** says it authenticated with (already
//! reduced to a safe label: `shared-token`, `no-auth`, `unknown`, or a plain
//! user name). That is raw material. This module is the judgement on top of
//! it: given the mode the operator *expects* the broker to run, which
//! connections are evidence that the broker is not doing that — a switch
//! that failed or silently reverted, or a broker that authenticates nobody.
//!
//! Everything here is pure (no I/O, no clock), so the whole table is tested
//! without a broker or a database.
//!
//! # The claimed role is client-supplied
//!
//! The role comes from the `kanade-<role>[/identity]` name the connection
//! announces, and a client chooses its own name. A mismatch between that name
//! and the user the broker authenticated is therefore evidence of
//! misconfiguration or abuse, and a **match proves nothing**: a host holding
//! the agent credential can announce itself as anything, and one announcing
//! `kanade-agent` while presenting the agent credential is simply behaving.
//! The authenticated user is the broker's word; the role is only the
//! connection's own claim to be compared against it.
//!
//! # What a finding never carries
//!
//! A finding is a kind, a severity and (once grouped) a count and a bounded
//! sample of registered host names. It never carries the reported user name,
//! the connection name, or anything the broker sent, so there is no path by
//! which a credential-shaped value could be stored or served from here — the
//! input is the already-classified label, and even that is not copied out.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Duration, Utc};
use kanade_shared::nats_client::{NatsRole, parse_client_name};
use kanade_shared::wire::NatsAuthMode;

use super::nats_conns::{LABEL_NO_AUTH, LABEL_SHARED_TOKEN, LABEL_UNKNOWN};

/// How long findings are held back after the expected mode is changed.
///
/// Switching a broker's authentication re-authenticates every connection:
/// each client is dropped and reconnects with backoff, and the monitoring
/// poll only looks once a minute. For a few polls the broker's view is a mix
/// of old and new, and every straggler would read as a violation of the mode
/// that was just chosen. Five minutes covers several polls plus the clients'
/// reconnect backoff while staying short enough that a switch that genuinely
/// did not take is reported promptly. While the window is open nothing new
/// is raised; notices already open can still be updated and resolved.
pub const MODE_SWITCH_GRACE: Duration = Duration::minutes(5);

/// Most host names carried per group. A whole fleet reverting at once is one
/// group with a count, not thousands of names.
pub const MAX_SAMPLE_HOSTS: usize = 5;

/// The user the shipped `users` broker config defines for each role. Kept
/// next to a test that reads that config, so a rename there is caught here.
pub const ROLE_USER_AGENT: &str = "agent";
pub const ROLE_USER_BACKEND: &str = "backend";
pub const ROLE_USER_CLI: &str = "breakglass";

/// The NATS user that belongs to `role`.
pub fn role_user(role: NatsRole) -> &'static str {
    match role {
        NatsRole::Agent => ROLE_USER_AGENT,
        NatsRole::Backend => ROLE_USER_BACKEND,
        NatsRole::Cli => ROLE_USER_CLI,
    }
}

/// What a connection claims to be, from its announced name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claimed {
    Role(NatsRole),
    /// A `kanade-` name whose role is none of ours.
    Unrecognised,
    /// No name at all. A raw client that sets none is exactly what an open
    /// broker lets in, so it is evaluated rather than skipped.
    Unnamed,
}

impl Claimed {
    /// `None` for a name that is not a kanade name at all (an operator's
    /// `nats` CLI, the client library's short-lived credential probe, other
    /// tooling): those are not part of the fleet's own identity scheme, and
    /// the backend's own connection is always there to reveal an open broker.
    pub fn from_name(name: Option<&str>) -> Option<Self> {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Some(Claimed::Unnamed);
        };
        let parsed = parse_client_name(name)?;
        Some(match parsed.role {
            r if r == NatsRole::Agent.as_str() => Claimed::Role(NatsRole::Agent),
            r if r == NatsRole::Backend.as_str() => Claimed::Role(NatsRole::Backend),
            r if r == NatsRole::Cli.as_str() => Claimed::Role(NatsRole::Cli),
            _ => Claimed::Unrecognised,
        })
    }

    /// Fixed vocabulary used as the notice subject. Never the raw role
    /// string, so a hostile name cannot mint subjects.
    pub fn subject(self) -> &'static str {
        match self {
            Claimed::Role(r) => r.as_str(),
            Claimed::Unrecognised => "other",
            Claimed::Unnamed => "unnamed",
        }
    }
}

/// What the broker reported for a connection, after
/// [`classify`](super::nats_conns) has made it safe to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reported {
    SharedToken,
    NoAuth,
    Unknown,
    User(String),
}

impl Reported {
    pub fn from_label(label: &str) -> Self {
        match label {
            LABEL_SHARED_TOKEN => Reported::SharedToken,
            LABEL_NO_AUTH => Reported::NoAuth,
            LABEL_UNKNOWN => Reported::Unknown,
            user => Reported::User(user.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    /// The broker authenticated nobody.
    BrokerOpen,
    /// A connection got in on the shared token although `users` is expected.
    TokenReverted,
    /// A connection authenticated as a user that is not its claimed role's.
    RoleUserMismatch,
    /// The credential could not be named, so it cannot be judged.
    CredentialUnnameable,
}

impl FindingKind {
    #[cfg(test)]
    pub const ALL: [FindingKind; 4] = [
        FindingKind::BrokerOpen,
        FindingKind::TokenReverted,
        FindingKind::RoleUserMismatch,
        FindingKind::CredentialUnnameable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::BrokerOpen => "broker_open",
            FindingKind::TokenReverted => "token_reverted",
            FindingKind::RoleUserMismatch => "role_user_mismatch",
            FindingKind::CredentialUnnameable => "credential_unnameable",
        }
    }

    /// The one place severity is decided.
    pub fn severity(self) -> Severity {
        match self {
            FindingKind::BrokerOpen | FindingKind::TokenReverted => Severity::Critical,
            FindingKind::RoleUserMismatch => Severity::Warning,
            FindingKind::CredentialUnnameable => Severity::Info,
        }
    }

    /// Ledger key, namespaced so the notice ledger can hold other sources.
    pub fn notice_kind(self) -> String {
        format!("{NOTICE_PREFIX}{}", self.as_str())
    }
}

/// Prefix of every notice kind this audit owns.
pub const NOTICE_PREFIX: &str = "nats_auth.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    pub severity: Severity,
}

impl Finding {
    fn of(kind: FindingKind) -> Option<Self> {
        Some(Finding {
            kind,
            severity: kind.severity(),
        })
    }
}

/// Judge one connection against the expected mode.
///
/// | reported       | `token` expected | `users` expected                     |
/// |----------------|------------------|--------------------------------------|
/// | `no-auth`      | `broker_open`    | `broker_open`                        |
/// | `shared-token` | fine             | `token_reverted`                     |
/// | `unknown`      | fine             | `credential_unnameable`              |
/// | a user name    | fine             | fine only if it is the claimed role's |
///
/// A user name under `token` is not judged: it means the broker already runs
/// `users` and the setting is merely stale, which is not a security
/// condition. Under `users`, a role that is not one of ours (or no role)
/// matches no user, so any user name there is a mismatch. See the module docs
/// for why the claimed role is only a claim.
pub fn evaluate(mode: NatsAuthMode, claimed: Claimed, reported: &Reported) -> Option<Finding> {
    match (mode, reported) {
        (_, Reported::NoAuth) => Finding::of(FindingKind::BrokerOpen),
        (NatsAuthMode::Token, _) => None,
        (NatsAuthMode::Users, Reported::SharedToken) => Finding::of(FindingKind::TokenReverted),
        (NatsAuthMode::Users, Reported::Unknown) => Finding::of(FindingKind::CredentialUnnameable),
        (NatsAuthMode::Users, Reported::User(name)) => match claimed {
            Claimed::Role(r) if role_user(r) == name.as_str() => None,
            _ => Finding::of(FindingKind::RoleUserMismatch),
        },
    }
}

/// One live connection as the poll saw it. `label` is the output of the
/// projector's classifier, never the broker's raw field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub claimed: Claimed,
    /// The host identity the connection announced. Claimed, so it is only
    /// ever named in a sample if it is a registered pc_id.
    pub pc_id: Option<String>,
    pub label: String,
}

/// Connections sharing a finding kind and a claimed-role bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub kind: FindingKind,
    pub subject: &'static str,
    /// Connections in the group (not distinct hosts).
    pub count: usize,
    /// Sorted, deduplicated, at most [`MAX_SAMPLE_HOSTS`] registered hosts.
    pub sample_hosts: Vec<String>,
}

/// Evaluate every observed connection and fold the findings into groups by
/// (kind, claimed-role bucket). One noisy connection, or the whole fleet
/// reverting at once, yields at most `kinds × buckets` groups.
///
/// `registered` is the set of known pc_ids: a connection's pc_id is claimed
/// by the host, so an unregistered one is counted but never named.
pub fn group(
    mode: NatsAuthMode,
    observed: &[Observed],
    registered: &HashSet<String>,
) -> Vec<Group> {
    let mut acc: BTreeMap<(FindingKind, &'static str), (usize, BTreeSet<String>)> = BTreeMap::new();
    for o in observed {
        let Some(f) = evaluate(mode, o.claimed, &Reported::from_label(&o.label)) else {
            continue;
        };
        let entry = acc.entry((f.kind, o.claimed.subject())).or_default();
        entry.0 += 1;
        if let (Claimed::Role(NatsRole::Agent), Some(pc)) = (o.claimed, o.pc_id.as_deref())
            && registered.contains(pc)
        {
            entry.1.insert(pc.to_string());
        }
    }
    acc.into_iter()
        .map(|((kind, subject), (count, hosts))| Group {
            kind,
            subject,
            count,
            sample_hosts: hosts.into_iter().take(MAX_SAMPLE_HOSTS).collect(),
        })
        .collect()
}

/// Whether findings are currently held back after a mode change.
///
/// A timestamp in the future is not a window: only the backend writes it, so
/// a future value is a clock step or a hand edit, and honouring it would let
/// it mute the audit indefinitely.
pub fn in_grace(now: DateTime<Utc>, changed_at: Option<DateTime<Utc>>) -> bool {
    changed_at.is_some_and(|c| {
        let elapsed = now - c;
        elapsed >= Duration::zero() && elapsed < MODE_SWITCH_GRACE
    })
}

/// When the current window ends, if one is open.
pub fn grace_until(now: DateTime<Utc>, changed_at: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    in_grace(now, changed_at)
        .then(|| changed_at.map(|c| c + MODE_SWITCH_GRACE))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Claimed; 5] = [
        Claimed::Role(NatsRole::Agent),
        Claimed::Role(NatsRole::Backend),
        Claimed::Role(NatsRole::Cli),
        Claimed::Unrecognised,
        Claimed::Unnamed,
    ];

    fn kind(mode: NatsAuthMode, c: Claimed, r: Reported) -> Option<FindingKind> {
        evaluate(mode, c, &r).map(|f| f.kind)
    }

    #[test]
    fn severity_table_is_pinned() {
        let table = [
            (FindingKind::BrokerOpen, Severity::Critical),
            (FindingKind::TokenReverted, Severity::Critical),
            (FindingKind::RoleUserMismatch, Severity::Warning),
            (FindingKind::CredentialUnnameable, Severity::Info),
        ];
        for (k, s) in table {
            assert_eq!(k.severity(), s, "{k:?}");
        }
        assert_eq!(table.len(), FindingKind::ALL.len());
    }

    #[test]
    fn token_mode_table_for_every_role_and_value() {
        for c in ROLES {
            assert_eq!(
                kind(NatsAuthMode::Token, c, Reported::NoAuth),
                Some(FindingKind::BrokerOpen),
                "{c:?}"
            );
            for r in [
                Reported::SharedToken,
                Reported::Unknown,
                Reported::User("agent".into()),
                Reported::User("not-a-role-user".into()),
                Reported::User("s3cr3t-looking-Value_0123456789".into()),
            ] {
                assert_eq!(kind(NatsAuthMode::Token, c, r.clone()), None, "{c:?} {r:?}");
            }
        }
    }

    #[test]
    fn users_mode_table_for_every_role_and_value() {
        for c in ROLES {
            assert_eq!(
                kind(NatsAuthMode::Users, c, Reported::NoAuth),
                Some(FindingKind::BrokerOpen),
                "{c:?}"
            );
            assert_eq!(
                kind(NatsAuthMode::Users, c, Reported::SharedToken),
                Some(FindingKind::TokenReverted),
                "{c:?}"
            );
            assert_eq!(
                kind(NatsAuthMode::Users, c, Reported::Unknown),
                Some(FindingKind::CredentialUnnameable),
                "{c:?}"
            );
            for r in [
                Reported::User("not-a-role-user".into()),
                Reported::User("s3cr3t-looking-Value_0123456789".into()),
            ] {
                assert_eq!(
                    kind(NatsAuthMode::Users, c, r.clone()),
                    Some(FindingKind::RoleUserMismatch),
                    "{c:?} {r:?}"
                );
            }
        }
        // Each role's own user is fine; every other role's user is not.
        let roles = [NatsRole::Agent, NatsRole::Backend, NatsRole::Cli];
        for claimed in roles {
            for user in roles {
                let got = kind(
                    NatsAuthMode::Users,
                    Claimed::Role(claimed),
                    Reported::User(role_user(user).into()),
                );
                if claimed == user {
                    assert_eq!(got, None, "{claimed:?} as {user:?}");
                } else {
                    assert_eq!(
                        got,
                        Some(FindingKind::RoleUserMismatch),
                        "{claimed:?} as {user:?}"
                    );
                }
            }
        }
        // A role user on a connection with no usable role never matches.
        for c in [Claimed::Unrecognised, Claimed::Unnamed] {
            for user in roles {
                assert_eq!(
                    kind(
                        NatsAuthMode::Users,
                        c,
                        Reported::User(role_user(user).into())
                    ),
                    Some(FindingKind::RoleUserMismatch),
                    "{c:?} as {user:?}"
                );
            }
        }
    }

    #[test]
    fn the_backend_user_on_an_agent_connection_is_a_mismatch() {
        let f = evaluate(
            NatsAuthMode::Users,
            Claimed::Role(NatsRole::Agent),
            &Reported::User(ROLE_USER_BACKEND.into()),
        )
        .unwrap();
        assert_eq!(f.kind, FindingKind::RoleUserMismatch);
        assert_eq!(f.severity, Severity::Warning);
    }

    #[test]
    fn role_users_match_the_shipped_users_config() {
        let conf = include_str!("../../../../configs/nats-server.users.conf");
        for u in [ROLE_USER_AGENT, ROLE_USER_BACKEND, ROLE_USER_CLI] {
            assert!(
                conf.contains(&format!("user: \"{u}\"")),
                "users config defines no user {u:?}"
            );
        }
    }

    #[test]
    fn claimed_role_comes_from_the_announced_name() {
        let c = |n| Claimed::from_name(Some(n));
        assert_eq!(c("kanade-agent/PC1"), Some(Claimed::Role(NatsRole::Agent)));
        assert_eq!(c("kanade-backend"), Some(Claimed::Role(NatsRole::Backend)));
        assert_eq!(c("kanade-cli"), Some(Claimed::Role(NatsRole::Cli)));
        assert_eq!(c("kanade-relay/PC1"), Some(Claimed::Unrecognised));
        assert_eq!(Claimed::from_name(None), Some(Claimed::Unnamed));
        assert_eq!(Claimed::from_name(Some("  ")), Some(Claimed::Unnamed));
        // Not ours: neither evaluated nor counted.
        assert_eq!(c("auth-probe"), None);
        assert_eq!(c("NATS CLI Version 0.1"), None);
    }

    #[test]
    fn subjects_are_a_fixed_vocabulary() {
        let subjects: Vec<_> = ROLES.iter().map(|c| c.subject()).collect();
        assert_eq!(subjects, ["agent", "backend", "cli", "other", "unnamed"]);
    }

    fn obs(claimed: Claimed, pc: Option<&str>, label: &str) -> Observed {
        Observed {
            claimed,
            pc_id: pc.map(str::to_string),
            label: label.to_string(),
        }
    }

    fn registered(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_whole_fleet_reverting_is_one_group_with_a_bounded_sample() {
        let agent = Claimed::Role(NatsRole::Agent);
        let ids: Vec<String> = (0..3000).map(|i| format!("PC{i:04}")).collect();
        let observed: Vec<_> = ids
            .iter()
            .map(|id| obs(agent, Some(id), LABEL_SHARED_TOKEN))
            .collect();
        let reg: HashSet<String> = ids.iter().cloned().collect();
        let groups = group(NatsAuthMode::Users, &observed, &reg);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.kind, FindingKind::TokenReverted);
        assert_eq!(g.subject, "agent");
        assert_eq!(g.count, 3000);
        assert_eq!(
            g.sample_hosts,
            ["PC0000", "PC0001", "PC0002", "PC0003", "PC0004"]
        );
    }

    #[test]
    fn a_noisy_connection_is_counted_once_per_connection_and_named_once() {
        let agent = Claimed::Role(NatsRole::Agent);
        let observed = vec![
            obs(agent, Some("PC1"), LABEL_NO_AUTH),
            obs(agent, Some("PC1"), LABEL_NO_AUTH),
        ];
        let g = group(NatsAuthMode::Token, &observed, &registered(&["PC1"]));
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].count, 2);
        assert_eq!(g[0].sample_hosts, ["PC1"]);
    }

    #[test]
    fn unregistered_and_non_agent_hosts_are_counted_but_never_named() {
        let observed = vec![
            obs(
                Claimed::Role(NatsRole::Agent),
                Some("<script>alert(1)</script>"),
                LABEL_NO_AUTH,
            ),
            obs(Claimed::Role(NatsRole::Backend), Some("PC1"), LABEL_NO_AUTH),
            obs(Claimed::Unnamed, None, LABEL_NO_AUTH),
        ];
        let groups = group(NatsAuthMode::Token, &observed, &registered(&["PC1"]));
        assert_eq!(groups.len(), 3);
        assert!(
            groups
                .iter()
                .all(|g| g.count == 1 && g.sample_hosts.is_empty())
        );
        let subjects: Vec<_> = groups.iter().map(|g| g.subject).collect();
        assert_eq!(subjects, ["agent", "backend", "unnamed"]);
    }

    #[test]
    fn a_clean_fleet_has_no_groups() {
        let agent = Claimed::Role(NatsRole::Agent);
        let observed = vec![
            obs(agent, Some("PC1"), "agent"),
            obs(Claimed::Role(NatsRole::Backend), None, "backend"),
        ];
        assert!(group(NatsAuthMode::Users, &observed, &registered(&["PC1"])).is_empty());
        let observed = vec![obs(agent, Some("PC1"), LABEL_SHARED_TOKEN)];
        assert!(group(NatsAuthMode::Token, &observed, &registered(&["PC1"])).is_empty());
    }

    #[test]
    fn groups_never_carry_the_reported_value() {
        let secret = "tok-9f8e7d6c5b4a-SECRET";
        let observed = vec![obs(Claimed::Role(NatsRole::Agent), Some("PC1"), secret)];
        let groups = group(NatsAuthMode::Users, &observed, &registered(&["PC1"]));
        assert_eq!(groups.len(), 1);
        assert!(!format!("{groups:?}").contains(secret));
    }

    #[test]
    fn grace_window_boundaries() {
        let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let s = Duration::seconds;
        let g = MODE_SWITCH_GRACE;
        let cases = [
            (None, t0, false),
            (Some(t0), t0, true),
            (Some(t0), t0 + s(1), true),
            (Some(t0), t0 + g - s(1), true),
            (Some(t0), t0 + g, false),
            (Some(t0), t0 + g + s(1), false),
            // Clock step backwards / hand-edited future value: not a window.
            (Some(t0 + s(60)), t0, false),
        ];
        for (changed, now, want) in cases {
            assert_eq!(in_grace(now, changed), want, "{changed:?} at {now}");
            assert_eq!(
                grace_until(now, changed).is_some(),
                want,
                "{changed:?} at {now}"
            );
        }
        assert_eq!(grace_until(t0, Some(t0)), Some(t0 + g));
    }

    #[test]
    fn the_grace_window_is_short() {
        assert!(MODE_SWITCH_GRACE <= Duration::minutes(10));
        assert!(MODE_SWITCH_GRACE >= Duration::minutes(3));
    }
}
