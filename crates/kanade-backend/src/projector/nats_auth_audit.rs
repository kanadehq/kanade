//! The broker-authentication audit: ties the connection poll to the notice
//! ledger and to the fleet health response.
//!
//! Each poll of [`nats_conns`](super::nats_conns) hands over what it saw.
//! [`plan`] (pure) turns that, the operator's expected mode and what the
//! ledger already holds into ledger writes; [`run_poll`] performs them and
//! records how the poll went, so the health response can say whether "no
//! findings" is a fresh answer or a blind spot.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use kanade_shared::kv::{BUCKET_SERVER_SETTINGS, KEY_SERVER_SETTINGS};
use kanade_shared::wire::ServerSettings;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::nats_auth_findings::{
    FindingKind, NOTICE_PREFIX, Observed, Severity, grace_until, group, in_grace,
};
use super::nats_conns::{EvidenceReason, LABEL_PENDING, LABEL_UNPROVEN};
use super::notice_ledger::{self, Desired, Op, Policy, Stored};

/// A recorded poll older than this is reported as `stale`: the projector
/// polls every minute, so three missed ticks means it has stopped answering.
const STALE_AFTER: Duration = Duration::minutes(3);

/// How the last poll went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
    /// The whole connection list was read and judged.
    Ok,
    /// The list was cut short (paging cap): findings can still be raised, but
    /// nothing is resolved because absence proves nothing.
    Incomplete,
    /// Some live connections have no proof of authentication acceptance.
    AuthenticationUncertain,
    /// The monitoring endpoint could not be read; the ledger was left as is.
    EndpointUnreadable,
    /// The expected mode could not be read, so nothing could be judged.
    SettingsUnreadable,
}

impl PollStatus {
    fn as_str(self) -> &'static str {
        match self {
            PollStatus::Ok => "ok",
            PollStatus::Incomplete => "incomplete",
            PollStatus::AuthenticationUncertain => "authentication_uncertain",
            PollStatus::EndpointUnreadable => "endpoint_unreadable",
            PollStatus::SettingsUnreadable => "settings_unreadable",
        }
    }
}

/// What one poll observed.
pub struct Poll<'a> {
    pub observed: &'a [Observed],
    pub registered: &'a HashSet<String>,
    /// Every page of the connection list was read.
    pub complete: bool,
    /// None for older callers without reason-level classification evidence.
    pub uncertainty_reasons: Option<&'a BTreeMap<EvidenceReason, usize>>,
}

impl Poll<'_> {
    fn uncertainty(&self) -> (usize, usize) {
        (
            self.observed
                .iter()
                .filter(|o| o.label == LABEL_PENDING)
                .count(),
            self.observed
                .iter()
                .filter(|o| o.label == LABEL_UNPROVEN)
                .count(),
        )
    }

    fn judged(&self) -> bool {
        self.complete && self.uncertainty() == (0, 0)
    }
}

/// The ledger writes this poll calls for.
pub fn plan(
    settings: &ServerSettings,
    poll: &Poll<'_>,
    stored: &[Stored],
    now: DateTime<Utc>,
) -> Vec<Op> {
    let mode = settings.effective_nats_auth_mode();
    let desired: Vec<Desired> = group(mode, poll.observed, poll.registered)
        .into_iter()
        .map(|g| Desired {
            kind: g.kind.notice_kind(),
            subject: g.subject.to_string(),
            severity: g.kind.severity().as_str().to_string(),
            count: g.count as i64,
            sample: g.sample_hosts,
        })
        .collect();
    let hold_resolve_kinds: Vec<String> = if poll.judged() {
        Vec::new()
    } else {
        FindingKind::ALL
            .iter()
            .map(|kind| kind.notice_kind())
            .collect()
    };
    notice_ledger::reconcile(
        stored,
        &desired,
        Policy {
            in_grace: in_grace(now, settings.nats_auth_mode_changed_at),
            complete: poll.complete,
            frozen_kinds: &[],
            hold_resolve_kinds: &hold_resolve_kinds,
        },
    )
}

/// Read the server settings from the KV bucket the SPA writes them to.
pub async fn read_settings(js: &async_nats::jetstream::Context) -> Result<ServerSettings> {
    let kv = js
        .get_key_value(BUCKET_SERVER_SETTINGS)
        .await
        .context("open server_settings KV")?;
    Ok(
        match kv
            .get(KEY_SERVER_SETTINGS)
            .await
            .context("get server_settings")?
        {
            Some(bytes) => serde_json::from_slice(&bytes).context("decode server_settings")?,
            None => ServerSettings::default(),
        },
    )
}

/// Registered pc_ids: the only host names a notice may carry.
pub async fn registered_pcs(pool: &SqlitePool) -> Result<HashSet<String>> {
    Ok(sqlx::query("SELECT pc_id FROM agents")
        .fetch_all(pool)
        .await
        .context("read agents")?
        .into_iter()
        .filter_map(|r| r.try_get::<String, _>("pc_id").ok())
        .collect())
}

/// Judge one poll and write what changed. Returns how many ledger writes
/// were made.
pub async fn run_poll(
    pool: &SqlitePool,
    settings: Result<ServerSettings>,
    poll: &Poll<'_>,
    now: DateTime<Utc>,
) -> Result<usize> {
    let settings = match settings {
        Ok(s) => s,
        Err(e) => {
            record_state(pool, PollStatus::SettingsUnreadable, now).await?;
            return Err(e);
        }
    };
    let stored = notice_ledger::load(pool, NOTICE_PREFIX).await?;
    let ops = plan(&settings, poll, &stored, now);
    let mut tx = pool.begin().await?;
    notice_ledger::apply_tx(&mut tx, &ops, now).await?;
    super::nats_auth_evidence::update(&mut tx, &settings, poll, &ops, now).await?;
    tx.commit().await?;
    let written = ops.len();
    let (pending, unproven) = poll.uncertainty();
    let status = if !poll.complete {
        PollStatus::Incomplete
    } else if pending + unproven > 0 {
        PollStatus::AuthenticationUncertain
    } else {
        PollStatus::Ok
    };
    record_details(
        pool,
        status,
        Some((pending, unproven)),
        poll.uncertainty_reasons,
        now,
    )
    .await?;
    Ok(written)
}

pub async fn record_state(pool: &SqlitePool, status: PollStatus, now: DateTime<Utc>) -> Result<()> {
    record_details(pool, status, None, None, now).await
}

// Aggregate poll metadata only; connection evidence belongs to the separate
// evidence ledger. Store an envelope in the existing TEXT field, accepting
// legacy plain statuses when reading, so no connection schema is coupled here.
#[derive(Serialize, Deserialize)]
struct RecordedState {
    status: String,
    pending_connections: Option<usize>,
    unproven_connections: Option<usize>,
    #[serde(default)]
    uncertainty_reasons: Option<BTreeMap<EvidenceReason, usize>>,
}

async fn record_details(
    pool: &SqlitePool,
    status: PollStatus,
    counts: Option<(usize, usize)>,
    reasons: Option<&BTreeMap<EvidenceReason, usize>>,
    now: DateTime<Utc>,
) -> Result<()> {
    super::nats_auth_evidence::prune(pool, now).await?;
    let state = RecordedState {
        status: status.as_str().into(),
        pending_connections: counts.map(|c| c.0),
        unproven_connections: counts.map(|c| c.1),
        uncertainty_reasons: reasons.cloned(),
    };
    sqlx::query(
        "INSERT INTO nats_auth_audit_state (id, status, polled_at) VALUES (1, ?, ?)
         ON CONFLICT(id) DO UPDATE SET status = excluded.status, polled_at = excluded.polled_at",
    )
    .bind(serde_json::to_string(&state)?)
    .bind(now)
    .execute(pool)
    .await
    .context("record nats auth audit state")?;
    Ok(())
}

/// The `nats_auth` block of the fleet health response.
///
/// Open findings only, grouped by kind and claimed-role bucket. Nothing in
/// here is read from the broker: kinds, subjects and severities are fixed
/// vocabularies, `sample_hosts` are registered pc_ids, and the rest are
/// counts and timestamps.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct NatsAuthHealth {
    /// `token` or `users`; `None` when the settings could not be read.
    pub expected_mode: Option<&'static str>,
    /// While set, newly appearing findings are held back after a mode change.
    pub grace_until: Option<DateTime<Utc>>,
    /// `ok`, `authentication_uncertain`, `incomplete`, `endpoint_unreadable`,
    /// `settings_unreadable`, `state_unreadable`, `stale` (no recent poll) or `pending` (none yet). Anything but `ok`
    /// means an empty `findings` list is not a clean bill of health.
    pub poll_status: String,
    pub polled_at: Option<DateTime<Utc>>,
    /// Counts from the last successful read; None when unavailable or stale.
    pub pending_connections: Option<usize>,
    pub unproven_connections: Option<usize>,
    /// Safe reason counts from this poll; null when unavailable or stale.
    pub uncertainty_reasons: Option<BTreeMap<EvidenceReason, usize>>,
    /// Connections covered by the open findings (the sum of their `count`s).
    pub open_total: i64,
    pub findings: Vec<FindingView>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct FindingView {
    pub occurrence_id: Option<i64>,
    pub kind: String,
    pub subject: String,
    pub severity: String,
    pub count: i64,
    pub sample_hosts: Vec<String>,
    pub first_seen_at: DateTime<Utc>,
}

fn severity_rank(s: &str) -> u8 {
    [Severity::Critical, Severity::Warning, Severity::Info]
        .iter()
        .position(|v| v.as_str() == s)
        .unwrap_or(usize::MAX) as u8
}

/// Assemble the health block. `settings` is `None` when it could not be read.
pub async fn health_block(
    pool: &SqlitePool,
    settings: Option<&ServerSettings>,
    now: DateTime<Utc>,
) -> NatsAuthHealth {
    let mut findings: Vec<FindingView> = notice_ledger::load(pool, NOTICE_PREFIX)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.resolved_at.is_none())
        .map(|s| FindingView {
            occurrence_id: None,
            kind: s
                .kind
                .strip_prefix(NOTICE_PREFIX)
                .unwrap_or(&s.kind)
                .to_string(),
            subject: s.subject,
            severity: s.severity,
            count: s.count,
            sample_hosts: s.sample,
            first_seen_at: s.first_seen_at,
        })
        .collect();
    let occurrences: Vec<(i64, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, kind, subject, first_seen_at FROM nats_auth_occurrences WHERE resolved_at IS NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for f in &mut findings {
        f.occurrence_id = occurrences
            .iter()
            .find(|(_, k, s, first)| {
                k == &format!("{NOTICE_PREFIX}{}", f.kind)
                    && s == &f.subject
                    && first == &f.first_seen_at
            })
            .map(|(id, _, _, _)| *id);
    }
    findings.sort_by(|a, b| {
        (severity_rank(&a.severity), &a.kind, &a.subject).cmp(&(
            severity_rank(&b.severity),
            &b.kind,
            &b.subject,
        ))
    });
    let state = sqlx::query("SELECT status, polled_at FROM nats_auth_audit_state WHERE id = 1")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let mut pending_connections = None;
    let mut unproven_connections = None;
    let mut uncertainty_reasons = None;
    let (poll_status, polled_at) = match state {
        None => ("pending".to_string(), None),
        Some(r) => {
            let at: Option<DateTime<Utc>> = r.try_get("polled_at").ok();
            let raw: String = r.try_get("status").unwrap_or_default();
            let recorded =
                serde_json::from_str::<RecordedState>(&raw).unwrap_or_else(|_| RecordedState {
                    status: if [
                        "ok",
                        "incomplete",
                        "endpoint_unreadable",
                        "settings_unreadable",
                    ]
                    .contains(&raw.as_str())
                    {
                        raw
                    } else {
                        "state_unreadable".into()
                    },
                    pending_connections: None,
                    unproven_connections: None,
                    uncertainty_reasons: None,
                });
            if at.is_some_and(|t| now - t <= STALE_AFTER) {
                pending_connections = recorded.pending_connections;
                unproven_connections = recorded.unproven_connections;
                uncertainty_reasons = recorded.uncertainty_reasons;
            }
            let status = recorded.status;
            match at {
                Some(t) if now - t > STALE_AFTER => ("stale".to_string(), Some(t)),
                _ => (status, at),
            }
        }
    };
    NatsAuthHealth {
        expected_mode: settings.map(|s| s.effective_nats_auth_mode().as_str()),
        grace_until: settings.and_then(|s| grace_until(now, s.nats_auth_mode_changed_at)),
        poll_status,
        polled_at,
        pending_connections,
        unproven_connections,
        uncertainty_reasons,
        open_total: findings.iter().map(|f| f.count).sum(),
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projector::nats_auth_findings::{Claimed, MODE_SWITCH_GRACE};
    use crate::projector::nats_conns::{LABEL_NO_AUTH, LABEL_SHARED_TOKEN, LABEL_UNKNOWN};
    use kanade_shared::nats_client::NatsRole;
    use kanade_shared::wire::NatsAuthMode;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn users() -> ServerSettings {
        ServerSettings {
            nats_auth_mode: Some(NatsAuthMode::Users),
            ..Default::default()
        }
    }

    fn agent(pc: &str, label: &str) -> Observed {
        Observed {
            connection: None,
            claimed: Claimed::Role(NatsRole::Agent),
            pc_id: Some(pc.into()),
            label: label.into(),
        }
    }

    fn reg() -> HashSet<String> {
        ["PC1", "PC2"].iter().map(|s| s.to_string()).collect()
    }

    fn poll<'a>(o: &'a [Observed], r: &'a HashSet<String>) -> Poll<'a> {
        Poll {
            observed: o,
            registered: r,
            complete: true,
            uncertainty_reasons: None,
        }
    }

    #[test]
    fn a_reverted_fleet_raises_one_critical_notice() {
        let o = [
            agent("PC1", LABEL_SHARED_TOKEN),
            agent("PC2", LABEL_SHARED_TOKEN),
        ];
        let r = reg();
        let ops = plan(&users(), &poll(&o, &r), &[], t0());
        assert_eq!(ops.len(), 1);
        let Op::Raise(d) = &ops[0] else {
            panic!("{ops:?}")
        };
        assert_eq!(d.kind, "nats_auth.token_reverted");
        assert_eq!(d.subject, "agent");
        assert_eq!(d.severity, "critical");
        assert_eq!(d.count, 2);
        assert_eq!(d.sample, ["PC1", "PC2"]);
    }

    #[test]
    fn grace_after_a_switch_holds_back_new_findings_then_releases() {
        let mut s = users();
        s.nats_auth_mode_changed_at = Some(t0());
        let o = [agent("PC1", LABEL_SHARED_TOKEN)];
        let r = reg();
        assert!(plan(&s, &poll(&o, &r), &[], t0() + Duration::minutes(1)).is_empty());
        assert_eq!(
            plan(&s, &poll(&o, &r), &[], t0() + MODE_SWITCH_GRACE).len(),
            1
        );
    }

    #[test]
    fn unnameable_credentials_are_reported_even_without_backend_proof() {
        // Without proof a user name reads as `unknown`; it is still a
        // low-severity finding, and nothing about it is withheld.
        let o = [agent("PC1", LABEL_UNKNOWN)];
        let r = reg();
        let ops = plan(&users(), &poll(&o, &r), &[], t0());
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], Op::Raise(d)
            if d.kind == "nats_auth.credential_unnameable" && d.severity == "info"));
    }

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn raise_dedupe_resolve_through_the_ledger_and_health() {
        let pool = pool().await;
        let r = reg();
        let bad = [agent("PC1", LABEL_SHARED_TOKEN)];
        let s = users();

        // Raised on first sight.
        assert_eq!(
            run_poll(&pool, Ok(s.clone()), &poll(&bad, &r), t0())
                .await
                .unwrap(),
            1
        );
        // Same poll again: deduplicated, no write.
        let t1 = t0() + Duration::minutes(1);
        assert_eq!(
            run_poll(&pool, Ok(s.clone()), &poll(&bad, &r), t1)
                .await
                .unwrap(),
            0
        );
        let h = health_block(&pool, Some(&s), t1).await;
        assert_eq!(h.expected_mode, Some("users"));
        assert_eq!(h.poll_status, "ok");
        assert_eq!(h.open_total, 1);
        assert_eq!(h.findings.len(), 1);
        assert_eq!(h.findings[0].kind, "token_reverted");
        assert_eq!(h.findings[0].first_seen_at, t0());
        let first_occurrence = h.findings[0].occurrence_id;
        assert!(first_occurrence.is_some());

        // Gone: resolved, and health shows nothing open.
        let t2 = t0() + Duration::minutes(2);
        assert_eq!(
            run_poll(&pool, Ok(s.clone()), &poll(&[], &r), t2)
                .await
                .unwrap(),
            1
        );
        let h = health_block(&pool, Some(&s), t2).await;
        assert!(h.findings.is_empty());
        assert_eq!(h.open_total, 0);

        // Back again: a fresh occurrence on the same row.
        let t3 = t0() + Duration::minutes(3);
        assert_eq!(
            run_poll(&pool, Ok(s.clone()), &poll(&bad, &r), t3)
                .await
                .unwrap(),
            1
        );
        let h = health_block(&pool, Some(&s), t3).await;
        assert_eq!(h.findings[0].first_seen_at, t3);
        assert!(h.findings[0].occurrence_id.is_some());
        assert_ne!(h.findings[0].occurrence_id, first_occurrence);
    }

    #[tokio::test]
    async fn an_incomplete_poll_keeps_open_notices_open() {
        let pool = pool().await;
        let r = reg();
        let s = users();
        let bad = [agent("PC1", LABEL_SHARED_TOKEN)];
        run_poll(&pool, Ok(s.clone()), &poll(&bad, &r), t0())
            .await
            .unwrap();
        let mut p = poll(&[], &r);
        p.complete = false;
        run_poll(&pool, Ok(s.clone()), &p, t0() + Duration::minutes(1))
            .await
            .unwrap();
        let h = health_block(&pool, Some(&s), t0() + Duration::minutes(1)).await;
        assert_eq!(h.poll_status, "incomplete");
        assert_eq!(h.findings.len(), 1);
    }

    #[tokio::test]
    async fn unreadable_settings_hold_the_ledger_and_say_so() {
        let pool = pool().await;
        let r = reg();
        let bad = [agent("PC1", LABEL_SHARED_TOKEN)];
        run_poll(&pool, Ok(users()), &poll(&bad, &r), t0())
            .await
            .unwrap();
        let res = run_poll(
            &pool,
            Err(anyhow::anyhow!("kv down")),
            &poll(&[], &r),
            t0() + Duration::minutes(1),
        )
        .await;
        assert!(res.is_err());
        let h = health_block(&pool, None, t0() + Duration::minutes(1)).await;
        assert_eq!(h.expected_mode, None);
        assert_eq!(h.poll_status, "settings_unreadable");
        assert_eq!(h.findings.len(), 1);
    }

    #[tokio::test]
    async fn a_silent_projector_reads_as_stale_and_a_new_one_as_pending() {
        let pool = pool().await;
        let h = health_block(&pool, Some(&users()), t0()).await;
        assert_eq!(h.poll_status, "pending");
        record_state(&pool, PollStatus::Ok, t0()).await.unwrap();
        let h = health_block(&pool, Some(&users()), t0() + Duration::minutes(10)).await;
        assert_eq!(h.poll_status, "stale");
    }

    #[tokio::test]
    async fn health_shape_and_no_secret_shaped_value_anywhere() {
        let pool = pool().await;
        let secret = "tok-0123456789abcdef-SECRET";
        let mut registered = reg();
        registered.insert("PC3".into());
        // A secret-shaped label and claimed host name, as a hostile or
        // misbehaving broker/host could produce.
        let o = [
            Observed {
                connection: None,
                claimed: Claimed::Role(NatsRole::Agent),
                pc_id: Some(secret.into()),
                label: secret.into(),
            },
            agent("PC3", LABEL_NO_AUTH),
        ];
        let s = users();
        run_poll(&pool, Ok(s.clone()), &poll(&o, &registered), t0())
            .await
            .unwrap();
        let h = health_block(&pool, Some(&s), t0()).await;
        let json = serde_json::to_value(&h).unwrap();
        let obj = json.as_object().unwrap();
        let mut keys: Vec<_> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "expected_mode",
                "findings",
                "grace_until",
                "open_total",
                "pending_connections",
                "poll_status",
                "polled_at",
                "uncertainty_reasons",
                "unproven_connections"
            ]
        );
        let mut fkeys: Vec<_> = json["findings"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fkeys.sort_unstable();
        assert_eq!(
            fkeys,
            [
                "count",
                "first_seen_at",
                "kind",
                "occurrence_id",
                "sample_hosts",
                "severity",
                "subject"
            ]
        );
        // Critical first.
        assert_eq!(json["findings"][0]["kind"], "broker_open");
        // Nothing the broker or a host claimed leaks into the response ...
        assert!(!json.to_string().contains(secret), "{json}");
        // ... or into storage.
        let stored: Vec<(String, String)> =
            sqlx::query_as("SELECT kind || subject, sample_json FROM backend_notices")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(!format!("{stored:?}").contains(secret));
    }
    #[tokio::test]
    async fn uncertain_auth_holds_resolution_but_allows_new_proven_critical_findings() {
        let pool = pool().await;
        let r = reg();
        let s = users();
        let old = [Observed {
            connection: None,
            claimed: Claimed::Unnamed,
            pc_id: None,
            label: LABEL_NO_AUTH.into(),
        }];
        run_poll(&pool, Ok(s.clone()), &poll(&old, &r), t0())
            .await
            .unwrap();
        let uncertain = [agent("PC1", LABEL_PENDING), agent("PC2", LABEL_UNPROVEN)];
        assert_eq!(
            run_poll(&pool, Ok(s.clone()), &poll(&uncertain, &r), t0())
                .await
                .unwrap(),
            0
        );
        let h = health_block(&pool, Some(&s), t0()).await;
        assert_eq!(h.poll_status, "authentication_uncertain");
        assert_eq!(
            (h.pending_connections, h.unproven_connections),
            (Some(1), Some(1))
        );
        assert_eq!(h.findings.len(), 1);
        assert_eq!(h.findings[0].kind, "broker_open");
        let history = super::super::nats_auth_evidence::history(&pool, None)
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].resolved_at.is_none());
        assert!(history[0].connections.is_empty());
        let mixed = [
            agent("PC1", LABEL_PENDING),
            agent("PC2", LABEL_SHARED_TOKEN),
        ];
        assert_eq!(
            run_poll(&pool, Ok(s.clone()), &poll(&mixed, &r), t0())
                .await
                .unwrap(),
            1
        );
        let h = health_block(&pool, Some(&s), t0()).await;
        assert_eq!(h.findings.len(), 2);
        assert!(h.findings.iter().all(|f| f.severity == "critical"));
        // Only a complete, judged poll may resolve either finding.
        run_poll(&pool, Ok(s.clone()), &poll(&[], &r), t0())
            .await
            .unwrap();
        let h = health_block(&pool, Some(&s), t0()).await;
        assert_eq!(h.poll_status, "ok");
        assert!(h.findings.is_empty());
        assert_eq!(
            (h.pending_connections, h.unproven_connections),
            (Some(0), Some(0))
        );
    }

    #[tokio::test]
    async fn uncertainty_counts_are_unavailable_on_unreadable_or_stale_polls() {
        let pool = pool().await;
        let r = reg();
        let s = users();
        let o = [agent("PC1", LABEL_UNPROVEN)];
        let mut incomplete = poll(&o, &r);
        incomplete.complete = false;
        let reasons = BTreeMap::from([(EvidenceReason::IncompleteMonitoring, 1)]);
        incomplete.uncertainty_reasons = Some(&reasons);
        run_poll(&pool, Ok(s.clone()), &incomplete, t0())
            .await
            .unwrap();
        let h = health_block(&pool, Some(&s), t0()).await;
        assert_eq!(h.poll_status, "incomplete");
        assert_eq!(h.unproven_connections, Some(1));
        assert_eq!(h.uncertainty_reasons.as_ref(), Some(&reasons));
        assert_eq!(
            serde_json::to_value(&h).unwrap()["uncertainty_reasons"]["incomplete_monitoring"],
            1
        );
        let h = health_block(&pool, Some(&s), t0() + STALE_AFTER + Duration::seconds(1)).await;
        assert_eq!(h.poll_status, "stale");
        assert_eq!(h.unproven_connections, None);
        assert_eq!(h.uncertainty_reasons, None);
        record_state(&pool, PollStatus::EndpointUnreadable, t0())
            .await
            .unwrap();
        let h = health_block(&pool, Some(&s), t0()).await;
        assert_eq!(h.poll_status, "endpoint_unreadable");
        assert_eq!(h.uncertainty_reasons, None);
        assert_eq!(
            (h.pending_connections, h.unproven_connections),
            (None, None)
        );
    }

    #[tokio::test]
    async fn legacy_statuses_remain_readable_and_corruption_is_unknown() {
        let pool = pool().await;
        for (raw, expected) in [
            ("ok", "ok"),
            ("incomplete", "incomplete"),
            ("broken", "state_unreadable"),
        ] {
            sqlx::query("INSERT INTO nats_auth_audit_state VALUES (1, ?, ?) ON CONFLICT(id) DO UPDATE SET status = excluded.status")
                .bind(raw).bind(t0()).execute(&pool).await.unwrap();
            let h = health_block(&pool, None, t0()).await;
            assert_eq!(h.poll_status, expected);
            assert_eq!(h.uncertainty_reasons, None);
        }
    }
}
