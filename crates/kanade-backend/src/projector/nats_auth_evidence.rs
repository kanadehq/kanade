//! Bounded, durable connection samples. No raw names, credentials or addresses.
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use kanade_shared::{nats_client::NatsRole, wire::ServerSettings};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection, SqlitePool};

use super::nats_auth_audit::Poll;
use super::nats_auth_findings::{Claimed, Reported, evaluate_observed};
use super::notice_ledger::Op;

const SAMPLE_HALF: usize = 10;
const MAX_BYTES: usize = 32 * 1024;
const RETENTION: Duration = Duration::days(30);

/// Only broker identifiers and parsed timestamps cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// Acquisition time travels only in memory; Sample persists first/last.
    #[serde(skip)]
    pub observed_at: Option<DateTime<Utc>>,
    /// Decimal string preserves all u64 bits in browser clients.
    pub cid: String,
    pub server_id: String,
    pub broker_start: DateTime<Utc>,
    pub broker_now: Option<DateTime<Utc>>,
    pub connection_start: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    #[serde(flatten)]
    pub connection: Connection,
    pub first_observed_at: DateTime<Utc>,
    pub last_observed_at: DateTime<Utc>,
    /// Fixed vocabulary; client supplied, not an authenticated identity.
    pub claimed_role: String,
    /// Registered pc_id only; still a client claim, not broker authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_host: Option<String>,
    pub credential_class: String,
    pub poll_complete: bool,
}

#[derive(Serialize)]
pub struct Occurrence {
    pub id: i64,
    pub kind: String,
    pub subject: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub count: i64,
    pub last_poll_complete: bool,
    pub sampled_out: bool,
    pub connections: Vec<Sample>,
}

// Timestamps are typed, roles/classes use fixed vocabularies, cid is decimal,
// and server_id is ASCII. Never truncate identifiers into ambiguous identities.
fn valid_identifiers(c: &Connection) -> bool {
    !c.server_id.is_empty()
        && c.server_id.len() <= 64
        && c.server_id.bytes().all(|b| b.is_ascii_alphanumeric())
        && c.cid
            .parse::<u64>()
            .is_ok_and(|id| id != 0 && id.to_string() == c.cid)
}

fn bounded_host(host: &str) -> String {
    host.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Admin history, bounded to the latest 100 occurrences per response.
pub async fn history(pool: &SqlitePool, before: Option<i64>) -> Result<Vec<Occurrence>> {
    let rows = sqlx::query("SELECT * FROM nats_auth_occurrences WHERE (? IS NULL OR id < ?) ORDER BY id DESC LIMIT 100")
        .bind(before).bind(before).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok(Occurrence {
                id: r.try_get("id")?,
                kind: r.try_get("kind")?,
                subject: r.try_get("subject")?,
                first_seen_at: r.try_get("first_seen_at")?,
                last_seen_at: r.try_get("last_seen_at")?,
                resolved_at: r.try_get("resolved_at")?,
                count: r.try_get("count")?,
                last_poll_complete: r.try_get("last_poll_complete")?,
                sampled_out: r.try_get("sampled_out")?,
                connections: serde_json::from_str(&r.try_get::<String, _>("evidence_json")?)?,
            })
        })
        .collect()
}

pub async fn update(
    conn: &mut SqliteConnection,
    settings: &ServerSettings,
    poll: &Poll<'_>,
    ops: &[Op],
    now: DateTime<Utc>,
) -> Result<()> {
    for op in ops {
        if let Op::Resolve { kind, subject } = op {
            sqlx::query("UPDATE nats_auth_occurrences SET resolved_at = ? WHERE kind = ? AND subject = ? AND resolved_at IS NULL")
                .bind(now).bind(kind).bind(subject).execute(&mut *conn).await?;
        }
    }
    // Includes unchanged groups: connection churn need not change counts or hosts.
    let rows = sqlx::query("SELECT kind, subject, count, first_seen_at FROM backend_notices WHERE kind LIKE 'nats_auth.%' AND resolved_at IS NULL")
        .fetch_all(&mut *conn).await?;
    for r in rows {
        let kind: String = r.try_get("kind")?;
        let subject: String = r.try_get("subject")?;
        let matching: Vec<_> = poll
            .observed
            .iter()
            .filter(|o| {
                evaluate_observed(settings.effective_nats_auth_mode(), o)
                    .is_some_and(|f| f.kind.notice_kind() == kind && o.claimed.subject() == subject)
            })
            .collect();
        if matching.is_empty() {
            continue;
        }
        sqlx::query("INSERT INTO nats_auth_occurrences (kind, subject, count, first_seen_at, last_seen_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING")
            .bind(&kind).bind(&subject).bind(r.try_get::<i64, _>("count")?)
            .bind(r.try_get::<DateTime<Utc>, _>("first_seen_at")?).bind(now).execute(&mut *conn).await?;
        let row = sqlx::query("SELECT id, evidence_json, sampled_out FROM nats_auth_occurrences WHERE kind = ? AND subject = ? AND resolved_at IS NULL")
            .bind(&kind).bind(&subject).fetch_one(&mut *conn).await?;
        let mut samples: Vec<Sample> =
            serde_json::from_str(&row.try_get::<String, _>("evidence_json")?)?;
        samples.retain(|s| s.last_observed_at >= now - RETENTION);
        // A retained tail may be temporarily evicted before its turn in this
        // poll. Keep these 20 originals so that does not reset first observation.
        let previous = samples.clone();
        let mut sampled_out: bool = row.try_get("sampled_out")?;
        for o in matching {
            let Some(c) = &o.connection else {
                continue;
            };
            if !valid_identifiers(c) {
                continue;
            }
            let observed_at = c.observed_at.unwrap_or(now);
            if let Some(s) = samples.iter_mut().find(|s| {
                s.connection.cid == c.cid
                    && s.connection.server_id == c.server_id
                    && s.connection.broker_start == c.broker_start
            }) {
                s.last_observed_at = observed_at;
                s.poll_complete = poll.complete;
                s.connection = c.clone();
            } else if let Some(old) = previous.iter().find(|s| {
                s.connection.cid == c.cid
                    && s.connection.server_id == c.server_id
                    && s.connection.broker_start == c.broker_start
            }) {
                let mut sample = old.clone();
                sample.connection = c.clone();
                sample.last_observed_at = observed_at;
                sample.poll_complete = poll.complete;
                samples.push(sample);
            } else {
                samples.push(Sample {
                    connection: c.clone(),
                    first_observed_at: observed_at,
                    last_observed_at: observed_at,
                    claimed_role: o.claimed.subject().into(),
                    claimed_host: o
                        .pc_id
                        .as_deref()
                        .filter(|pc| {
                            o.claimed == Claimed::Role(NatsRole::Agent)
                                && poll.registered.contains(*pc)
                        })
                        .map(bounded_host),
                    poll_complete: poll.complete,
                    credential_class: match Reported::from_label(&o.label) {
                        Reported::NoAuth => "no-auth",
                        Reported::SharedToken => "shared-token",
                        Reported::Unknown => "unknown",
                        Reported::Pending => "auth-pending",
                        Reported::Unproven => "auth-unproven",
                        Reported::User(_) => "user",
                    }
                    .into(),
                });
            }
            // Preserve the first ten and the ten most recently observed others.
            if samples.len() > SAMPLE_HALF * 2 {
                let remove = (SAMPLE_HALF..samples.len())
                    .min_by_key(|i| samples[*i].last_observed_at)
                    .unwrap();
                samples.remove(remove);
                sampled_out = true;
            }
        }
        let mut json = serde_json::to_string(&samples)?;
        while json.len() > MAX_BYTES {
            samples.pop();
            sampled_out = true;
            json = serde_json::to_string(&samples)?;
        }
        sqlx::query("UPDATE nats_auth_occurrences SET evidence_json = ?, sampled_out = ?, last_seen_at = ?, count = ?, last_poll_complete = ? WHERE id = ?")
            .bind(json).bind(sampled_out).bind(now).bind(r.try_get::<i64, _>("count")?).bind(poll.complete).bind(row.try_get::<i64, _>("id")?)
            .execute(&mut *conn).await?;
    }
    Ok(())
}

/// Run at startup and on every audit tick, including failed polls.
pub async fn prune(pool: &SqlitePool, now: DateTime<Utc>) -> Result<()> {
    let cutoff = now - RETENTION;
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM nats_auth_occurrences WHERE resolved_at IS NOT NULL AND (julianday(resolved_at) < julianday(?) OR id NOT IN (SELECT id FROM nats_auth_occurrences WHERE resolved_at IS NOT NULL ORDER BY id DESC LIMIT 1000))")
        .bind(cutoff).execute(&mut *tx).await?;
    sqlx::query("UPDATE nats_auth_occurrences SET evidence_json = (SELECT json_group_array(json(value)) FROM json_each(evidence_json) WHERE julianday(json_extract(value, '$.last_observed_at')) >= julianday(?)) WHERE EXISTS (SELECT 1 FROM json_each(evidence_json) WHERE julianday(json_extract(value, '$.last_observed_at')) < julianday(?))")
        .bind(cutoff).bind(cutoff).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{nats_auth_audit::run_poll, nats_auth_findings::Observed};
    use super::*;
    use std::collections::HashSet;

    fn now() -> DateTime<Utc> {
        "2026-10-18T02:24:58Z".parse().unwrap()
    }
    fn observed(cid: u64, epoch: i64) -> Observed {
        Observed {
            claimed: Claimed::Unnamed,
            pc_id: None,
            label: "no-auth".into(),
            connection: Some(Connection {
                observed_at: None,
                cid: cid.to_string(),
                server_id: "BROKER".into(),
                broker_start: now() + Duration::seconds(epoch),
                broker_now: Some(now()),
                connection_start: None,
                last_activity: None,
            }),
        }
    }
    async fn pool() -> SqlitePool {
        let p = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&p).await.unwrap();
        p
    }
    async fn poll(p: &SqlitePool, obs: &[Observed], t: DateTime<Utc>, complete: bool) {
        run_poll(
            p,
            Ok(ServerSettings::default()),
            &Poll {
                observed: obs,
                registered: &HashSet::new(),
                complete,
            },
            t,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn anonymous_churn_resolution_recurrence_and_reused_cid_have_durable_history() {
        let p = pool().await;
        poll(&p, &[observed(1, -60)], now(), true).await;
        // Same group/count/hosts; a different connection must still be recorded.
        poll(&p, &[observed(2, -60)], now() + Duration::minutes(1), true).await;
        // A broker restart reuses cid; server_id by itself is insufficient.
        poll(&p, &[observed(1, 90)], now() + Duration::minutes(2), true).await;
        let h = history(&p, None).await.unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].connections.len(), 3);
        assert_eq!(h[0].connections[0].connection.cid, "1");
        poll(
            &p,
            &[observed(1, 90)],
            now() + Duration::seconds(121),
            false,
        )
        .await;
        let partial = history(&p, None).await.unwrap();
        assert!(!partial[0].last_poll_complete);
        assert!(!partial[0].connections.last().unwrap().poll_complete);

        assert_ne!(
            h[0].connections[0].connection.broker_start,
            h[0].connections[2].connection.broker_start
        );
        poll(&p, &[], now() + Duration::minutes(3), false).await;
        assert!(history(&p, None).await.unwrap()[0].resolved_at.is_none());
        poll(&p, &[], now() + Duration::minutes(4), true).await;
        poll(&p, &[observed(3, 90)], now() + Duration::minutes(5), true).await;
        let h = history(&p, None).await.unwrap();
        assert_eq!(h.len(), 2);
        assert_ne!(h[0].id, h[1].id);
        assert!(h[1].resolved_at.is_some());
        assert_eq!(h[1].connections.len(), 3);
        assert_eq!(history(&p, Some(h[0].id)).await.unwrap()[0].id, h[1].id);
    }

    #[tokio::test]
    async fn head_tail_identity_and_resolved_samples_remain_stable() {
        let p = pool().await;
        let initial: Vec<_> = (1..=30).map(|id| observed(id, -60)).collect();
        poll(&p, &initial, now(), true).await;
        let h = history(&p, None).await.unwrap();
        let occurrence = h[0].id;
        let head = serde_json::to_value(&h[0].connections[..10]).unwrap();
        // Refresh the retained tail in reverse order, then add ten new peers.
        let next: Vec<_> = (21..=30)
            .rev()
            .chain(31..=40)
            .map(|id| observed(id, -60))
            .collect();
        poll(&p, &next, now() + Duration::minutes(1), true).await;
        let h = history(&p, None).await.unwrap();
        assert_eq!(h[0].id, occurrence);
        assert_eq!(serde_json::to_value(&h[0].connections[..10]).unwrap(), head);
        let ids: Vec<_> = h[0]
            .connections
            .iter()
            .map(|s| s.connection.cid.clone())
            .collect();
        assert_eq!(
            ids,
            (1..=10)
                .chain(31..=40)
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
        );
        let samples = serde_json::to_value(&h[0].connections).unwrap();
        poll(&p, &[], now() + Duration::minutes(2), true).await;
        poll(&p, &[observed(50, -60)], now() + Duration::minutes(3), true).await;
        let h = history(&p, None).await.unwrap();
        assert_eq!(h[1].id, occurrence);
        assert!(h[1].resolved_at.is_some());
        assert_eq!(serde_json::to_value(&h[1].connections).unwrap(), samples);
    }

    #[tokio::test]
    async fn host_claims_require_registration_and_all_text_fields_are_bounded() {
        let p = pool().await;
        let host = format!("PC\n\t{}", "界".repeat(100));
        let mut obs: Vec<_> = (1..=3).map(|id| observed(id, -60)).collect();
        for (o, pc) in obs
            .iter_mut()
            .zip([host.as_str(), "unknown-secret", host.as_str()])
        {
            o.claimed = Claimed::Role(NatsRole::Agent);
            o.pc_id = Some(pc.into());
        }
        // Registration alone is insufficient for a non-agent role claim.
        obs[2].claimed = Claimed::Unnamed;
        run_poll(
            &p,
            Ok(ServerSettings::default()),
            &Poll {
                observed: &obs,
                registered: &HashSet::from([host]),
                complete: true,
            },
            now(),
        )
        .await
        .unwrap();
        let h = history(&p, None).await.unwrap();
        let samples: Vec<_> = h.iter().flat_map(|o| &o.connections).collect();
        assert_eq!(samples.len(), 3);
        let named = samples.iter().find(|s| s.connection.cid == "1").unwrap();
        let value = named.claimed_host.as_deref().unwrap();
        assert_eq!(value.chars().count(), 64);
        assert!(!value.chars().any(char::is_control));
        assert!(
            samples
                .iter()
                .filter(|s| s.connection.cid != "1")
                .all(|s| s.claimed_host.is_none())
        );
        assert!(
            !serde_json::to_string(&h)
                .unwrap()
                .contains("unknown-secret")
        );
        // Legacy JSON remains readable without the additive optional field.
        let legacy = serde_json::to_value(observed(4, -60).connection.unwrap()).unwrap();
        let mut legacy = legacy.as_object().unwrap().clone();
        legacy.insert("first_observed_at".into(), serde_json::json!(now()));
        legacy.insert("last_observed_at".into(), serde_json::json!(now()));
        legacy.insert("claimed_role".into(), serde_json::json!("unnamed"));
        legacy.insert("credential_class".into(), serde_json::json!("no-auth"));
        legacy.insert("poll_complete".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<Sample>(legacy.into())
                .unwrap()
                .claimed_host
                .is_none()
        );
        for invalid in ["BROKER\n".to_string(), "B".repeat(65)] {
            let mut c = observed(5, -60).connection.unwrap();
            c.server_id = invalid;
            assert!(!valid_identifiers(&c));
        }
        let mut c = observed(5, -60).connection.unwrap();
        for invalid in ["1\n", "01", "18446744073709551616"] {
            c.cid = invalid.into();
            assert!(!valid_identifiers(&c));
        }
    }

    #[tokio::test]
    async fn sample_timestamps_use_acquisition_time_not_delayed_audit_processing() {
        let p = pool().await;
        let mut o = observed(1, -60);
        o.connection.as_mut().unwrap().observed_at = Some(now());
        poll(&p, &[o], now() + Duration::minutes(1), true).await;
        let h = history(&p, None).await.unwrap();
        assert_eq!(h[0].connections[0].first_observed_at, now());
        assert_eq!(h[0].connections[0].last_observed_at, now());
    }

    #[tokio::test]
    async fn evidence_survives_pool_restart_and_ledger_failure_rolls_back_together() {
        let dir = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("audit.sqlite"))
            .create_if_missing(true);
        let p = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&p).await.unwrap();
        poll(&p, &[observed(1, -60)], now(), true).await;
        let original_id = history(&p, None).await.unwrap()[0].id;
        p.close().await;
        let p = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        assert_eq!(history(&p, None).await.unwrap()[0].id, original_id);
        poll(&p, &[observed(1, -60)], now() + Duration::minutes(1), true).await;
        assert_eq!(history(&p, None).await.unwrap()[0].connections.len(), 1);
        // Fail the history half after the ledger's resolve statement.
        sqlx::query("CREATE TRIGGER fail_resolution BEFORE UPDATE ON nats_auth_occurrences BEGIN SELECT RAISE(ABORT, 'test failure'); END").execute(&p).await.unwrap();
        let result = run_poll(
            &p,
            Ok(ServerSettings::default()),
            &Poll {
                observed: &[],
                registered: &HashSet::new(),
                complete: true,
            },
            now() + Duration::minutes(2),
        )
        .await;
        assert!(result.is_err());
        let resolved: Option<String> =
            sqlx::query_scalar("SELECT resolved_at FROM backend_notices")
                .fetch_one(&p)
                .await
                .unwrap();
        assert!(resolved.is_none());
        assert!(history(&p, None).await.unwrap()[0].resolved_at.is_none());
        p.close().await;
    }

    #[tokio::test]
    async fn samples_bytes_expiry_and_resolved_history_are_bounded() {
        let p = pool().await;
        let obs: Vec<_> = (1..=100).map(|id| observed(id, -60)).collect();
        poll(&p, &obs, now(), true).await;
        let h = history(&p, None).await.unwrap();
        assert_eq!(h[0].connections.len(), 20);
        assert!(h[0].sampled_out);
        let ids: Vec<_> = h[0]
            .connections
            .iter()
            .map(|s| s.connection.cid.parse::<u64>().unwrap())
            .collect();
        assert_eq!(ids, (1..=10).chain(91..=100).collect::<Vec<_>>());
        poll(&p, &obs, now() + Duration::minutes(1), true).await;
        let again = history(&p, None).await.unwrap();
        assert!(
            again[0]
                .connections
                .iter()
                .all(|s| s.first_observed_at == now())
        );
        assert!(
            again[0]
                .connections
                .iter()
                .all(|s| s.last_observed_at == now() + Duration::minutes(1))
        );

        let bytes: i64 = sqlx::query_scalar(
            "SELECT length(CAST(evidence_json AS BLOB)) FROM nats_auth_occurrences",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert!(bytes < MAX_BYTES as i64);
        assert!(
            sqlx::query("UPDATE nats_auth_occurrences SET evidence_json = ?")
                .bind("x".repeat(MAX_BYTES + 1))
                .execute(&p)
                .await
                .is_err()
        );
        // Active occurrences also lose expired evidence, without losing the alert.
        super::super::nats_auth_audit::record_state(
            &p,
            super::super::nats_auth_audit::PollStatus::EndpointUnreadable,
            now() + Duration::days(31),
        )
        .await
        .unwrap();
        assert!(history(&p, None).await.unwrap()[0].connections.is_empty());
        poll(&p, &[], now() + Duration::days(31), true).await;
        prune(&p, now() + Duration::days(62)).await.unwrap();
        assert!(history(&p, None).await.unwrap().is_empty());
        for _ in 0..1005 {
            sqlx::query("INSERT INTO nats_auth_occurrences (kind, subject, count, first_seen_at, last_seen_at, resolved_at) VALUES ('nats_auth.broker_open', 'unnamed', 1, ?, ?, ?)")
                .bind(now()).bind(now()).bind(now()).execute(&p).await.unwrap();
        }
        prune(&p, now()).await.unwrap();
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM nats_auth_occurrences")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(total, 1000);
    }

    #[tokio::test]
    async fn additive_migration_preserves_legacy_notices_without_fabricating_evidence() {
        let p = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20260930000000_backend_notices.sql"
        ))
        .execute(&p)
        .await
        .unwrap();
        sqlx::query("INSERT INTO backend_notices (kind, subject, severity, count, sample_json, first_seen_at, updated_at, resolved_at) VALUES ('nats_auth.broker_open', 'unnamed', 'critical', 1, '[]', ?, ?, ?)")
            .bind(now()).bind(now() + Duration::minutes(1)).bind(now() + Duration::minutes(1)).execute(&p).await.unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261018000000_nats_auth_evidence.sql"
        ))
        .execute(&p)
        .await
        .unwrap();
        assert!(history(&p, None).await.unwrap().is_empty());
        let legacy = super::super::notice_ledger::load(&p, "nats_auth.")
            .await
            .unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].count, 1);
        assert_eq!(legacy[0].first_seen_at, now());
        assert_eq!(legacy[0].resolved_at, Some(now() + Duration::minutes(1)));
    }

    #[tokio::test]
    async fn legacy_notices_are_not_backfilled_and_secret_labels_and_claims_never_enter_evidence() {
        let p = pool().await;
        sqlx::query("INSERT INTO backend_notices (kind, subject, severity, count, sample_json, first_seen_at, updated_at) VALUES ('nats_auth.broker_open','unnamed','critical',1,'[]',?,?)")
            .bind(now()).bind(now()).execute(&p).await.unwrap();
        assert!(history(&p, None).await.unwrap().is_empty());
        let secret = "credential-should-never-be-recorded";
        let mut o = observed(4, -60);
        o.label = secret.into();
        o.pc_id = Some(secret.into());
        let settings = ServerSettings {
            nats_auth_mode: Some(kanade_shared::wire::NatsAuthMode::Users),
            ..ServerSettings::default()
        };
        run_poll(
            &p,
            Ok(settings),
            &Poll {
                observed: &[o],
                registered: &HashSet::new(),
                complete: true,
            },
            now(),
        )
        .await
        .unwrap();
        let json = serde_json::to_string(&history(&p, None).await.unwrap()).unwrap();
        assert!(!json.contains(secret));
        assert!(json.contains("\"credential_class\":\"user\""));
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT evidence_json FROM nats_auth_occurrences")
                .fetch_all(&p)
                .await
                .unwrap();
        assert!(!stored.join("").contains(secret));
    }
}
