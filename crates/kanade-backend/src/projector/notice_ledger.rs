//! A small ledger of conditions the backend raised for operators to see,
//! deduplicated by (kind, subject).
//!
//! The decision of what to write is [`reconcile`], a pure function from what
//! is stored and what is currently true; [`load`] and [`apply`] are the thin
//! SQL around it. Writing only on a difference matters here for the same
//! reason it does for `agents.nats_user`: the audit runs every minute, and a
//! steady state must not take SQLite's writer lock.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

/// A condition that is true right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub kind: String,
    pub subject: String,
    pub severity: String,
    pub count: i64,
    pub sample: Vec<String>,
}

/// A ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub kind: String,
    pub subject: String,
    pub severity: String,
    pub count: i64,
    pub sample: Vec<String>,
    pub first_seen_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Insert, or reopen a resolved row (resetting `first_seen_at`).
    Raise(Desired),
    /// An open row whose count, sample or severity moved.
    Update(Desired),
    Resolve {
        kind: String,
        subject: String,
    },
}

/// Conditions under which one reconcile runs.
#[derive(Debug, Clone, Copy)]
pub struct Policy<'a> {
    /// A recent mode change is being absorbed: raise nothing new, but let
    /// updates and resolves through.
    pub in_grace: bool,
    /// The poll saw everything it should have. When false, absence proves
    /// nothing, so nothing is resolved.
    pub complete: bool,
    /// Kinds whose evidence is unreliable this poll: neither raised nor
    /// resolved, left exactly as they were.
    pub frozen_kinds: &'a [String],
}

/// Decide the writes that bring `stored` in line with `desired`.
///
/// `stored` must already be limited to the kinds the caller owns; a stored
/// open row with no matching `desired` entry is taken to have cleared.
pub fn reconcile(stored: &[Stored], desired: &[Desired], policy: Policy<'_>) -> Vec<Op> {
    let frozen = |kind: &str| policy.frozen_kinds.iter().any(|k| k == kind);
    let by_key: HashMap<(&str, &str), &Stored> = stored
        .iter()
        .map(|s| ((s.kind.as_str(), s.subject.as_str()), s))
        .collect();
    let mut ops = Vec::new();
    for d in desired.iter().filter(|d| !frozen(&d.kind)) {
        match by_key.get(&(d.kind.as_str(), d.subject.as_str())) {
            // New, or back after being resolved: a fresh occurrence.
            None
            | Some(Stored {
                resolved_at: Some(_),
                ..
            }) => {
                if !policy.in_grace {
                    ops.push(Op::Raise(d.clone()));
                }
            }
            Some(s) => {
                if s.severity != d.severity || s.count != d.count || s.sample != d.sample {
                    ops.push(Op::Update(d.clone()));
                }
            }
        }
    }
    if policy.complete {
        for s in stored
            .iter()
            .filter(|s| s.resolved_at.is_none() && !frozen(&s.kind))
        {
            let still = desired
                .iter()
                .any(|d| d.kind == s.kind && d.subject == s.subject);
            if !still {
                ops.push(Op::Resolve {
                    kind: s.kind.clone(),
                    subject: s.subject.clone(),
                });
            }
        }
    }
    ops
}

/// Every row (open or resolved) whose kind starts with `prefix`.
pub async fn load(pool: &SqlitePool, prefix: &str) -> Result<Vec<Stored>> {
    let rows = sqlx::query(
        "SELECT kind, subject, severity, count, sample_json, first_seen_at, resolved_at
           FROM backend_notices
          WHERE substr(kind, 1, ?) = ?",
    )
    .bind(prefix.len() as i64)
    .bind(prefix)
    .fetch_all(pool)
    .await
    .context("read backend_notices")?;
    rows.into_iter()
        .map(|r| {
            let sample_json: String = r.try_get("sample_json")?;
            Ok(Stored {
                kind: r.try_get("kind")?,
                subject: r.try_get("subject")?,
                severity: r.try_get("severity")?,
                count: r.try_get("count")?,
                sample: serde_json::from_str(&sample_json).unwrap_or_default(),
                first_seen_at: r.try_get("first_seen_at")?,
                resolved_at: r.try_get("resolved_at")?,
            })
        })
        .collect::<std::result::Result<_, sqlx::Error>>()
        .context("decode backend_notices")
}

/// Write `ops` in one transaction. Returns how many were applied.
pub async fn apply(pool: &SqlitePool, ops: &[Op], now: DateTime<Utc>) -> Result<usize> {
    if ops.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await.context("begin backend_notices tx")?;
    for op in ops {
        match op {
            Op::Raise(d) => {
                sqlx::query(
                    "INSERT INTO backend_notices
                         (kind, subject, severity, count, sample_json,
                          first_seen_at, updated_at, resolved_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
                     ON CONFLICT(kind, subject) DO UPDATE SET
                         severity = excluded.severity,
                         count = excluded.count,
                         sample_json = excluded.sample_json,
                         first_seen_at = excluded.first_seen_at,
                         updated_at = excluded.updated_at,
                         resolved_at = NULL",
                )
                .bind(&d.kind)
                .bind(&d.subject)
                .bind(&d.severity)
                .bind(d.count)
                .bind(sample_json(&d.sample))
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("raise notice {}", d.kind))?;
            }
            Op::Update(d) => {
                sqlx::query(
                    "UPDATE backend_notices
                        SET severity = ?, count = ?, sample_json = ?, updated_at = ?
                      WHERE kind = ? AND subject = ? AND resolved_at IS NULL",
                )
                .bind(&d.severity)
                .bind(d.count)
                .bind(sample_json(&d.sample))
                .bind(now)
                .bind(&d.kind)
                .bind(&d.subject)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("update notice {}", d.kind))?;
            }
            Op::Resolve { kind, subject } => {
                sqlx::query(
                    "UPDATE backend_notices
                        SET resolved_at = ?, updated_at = ?
                      WHERE kind = ? AND subject = ? AND resolved_at IS NULL",
                )
                .bind(now)
                .bind(now)
                .bind(kind)
                .bind(subject)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("resolve notice {kind}"))?;
            }
        }
    }
    tx.commit().await.context("commit backend_notices tx")?;
    Ok(ops.len())
}

fn sample_json(sample: &[String]) -> String {
    serde_json::to_string(sample).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn d(kind: &str, subject: &str, count: i64, sample: &[&str]) -> Desired {
        Desired {
            kind: kind.into(),
            subject: subject.into(),
            severity: "critical".into(),
            count,
            sample: sample.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn s(of: &Desired, resolved: bool) -> Stored {
        Stored {
            kind: of.kind.clone(),
            subject: of.subject.clone(),
            severity: of.severity.clone(),
            count: of.count,
            sample: of.sample.clone(),
            first_seen_at: now(),
            resolved_at: resolved.then(now),
        }
    }

    const LIVE: Policy<'static> = Policy {
        in_grace: false,
        complete: true,
        frozen_kinds: &[],
    };

    #[test]
    fn new_condition_is_raised_once() {
        let want = d("k.a", "agent", 3, &["PC1"]);
        assert_eq!(
            reconcile(&[], std::slice::from_ref(&want), LIVE),
            [Op::Raise(want.clone())]
        );
        // The next poll sees it stored and unchanged: nothing to write.
        assert!(reconcile(&[s(&want, false)], &[want], LIVE).is_empty());
    }

    #[test]
    fn a_changed_count_or_sample_updates_without_reraising() {
        let old = d("k.a", "agent", 3, &["PC1"]);
        let now_ = d("k.a", "agent", 4, &["PC1", "PC2"]);
        assert_eq!(
            reconcile(&[s(&old, false)], std::slice::from_ref(&now_), LIVE),
            [Op::Update(now_)]
        );
    }

    #[test]
    fn a_cleared_condition_is_resolved_and_a_recurrence_is_raised_again() {
        let c = d("k.a", "agent", 1, &[]);
        assert_eq!(
            reconcile(&[s(&c, false)], &[], LIVE),
            [Op::Resolve {
                kind: "k.a".into(),
                subject: "agent".into()
            }]
        );
        // Already resolved and still gone: nothing.
        assert!(reconcile(&[s(&c, true)], &[], LIVE).is_empty());
        // Resolved, then true again: a fresh raise, not an update.
        assert_eq!(
            reconcile(&[s(&c, true)], std::slice::from_ref(&c), LIVE),
            [Op::Raise(c)]
        );
    }

    #[test]
    fn dedupe_is_by_kind_and_subject() {
        let a = d("k.a", "agent", 1, &[]);
        let b = d("k.a", "backend", 1, &[]);
        let ops = reconcile(&[s(&a, false)], &[a.clone(), b.clone()], LIVE);
        assert_eq!(ops, [Op::Raise(b)]);
    }

    #[test]
    fn grace_suppresses_raises_but_not_updates_or_resolves() {
        let policy = Policy {
            in_grace: true,
            ..LIVE
        };
        let new = d("k.new", "agent", 1, &[]);
        let reopened = d("k.old", "agent", 1, &[]);
        let open = d("k.open", "agent", 1, &[]);
        let open_now = d("k.open", "agent", 2, &[]);
        let gone = d("k.gone", "agent", 1, &[]);
        let stored = [s(&reopened, true), s(&open, false), s(&gone, false)];
        let ops = reconcile(&stored, &[new, reopened, open_now.clone()], policy);
        assert_eq!(
            ops,
            [
                Op::Update(open_now),
                Op::Resolve {
                    kind: "k.gone".into(),
                    subject: "agent".into()
                }
            ]
        );
    }

    #[test]
    fn an_incomplete_poll_resolves_nothing_but_still_raises() {
        let policy = Policy {
            complete: false,
            ..LIVE
        };
        let gone = d("k.gone", "agent", 1, &[]);
        let new = d("k.new", "agent", 1, &[]);
        let ops = reconcile(&[s(&gone, false)], std::slice::from_ref(&new), policy);
        assert_eq!(ops, [Op::Raise(new)]);
    }

    #[test]
    fn frozen_kinds_are_neither_raised_nor_resolved() {
        let frozen = ["k.f".to_string()];
        let policy = Policy {
            frozen_kinds: &frozen,
            ..LIVE
        };
        let open = d("k.f", "agent", 1, &[]);
        // Absent from `desired` (evidence unreliable): stays open.
        assert!(reconcile(&[s(&open, false)], &[], policy).is_empty());
        // Present but frozen: not raised, not updated.
        assert!(reconcile(&[], std::slice::from_ref(&open), policy).is_empty());
        assert!(reconcile(&[s(&open, false)], &[d("k.f", "agent", 9, &[])], policy).is_empty());
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
    async fn raise_update_resolve_reopen_round_trip_through_sql() {
        let pool = pool().await;
        let t = now();
        let first = d("nats_auth.x", "agent", 2, &["PC1", "PC2"]);
        apply(&pool, &[Op::Raise(first.clone())], t).await.unwrap();
        let rows = load(&pool, "nats_auth.").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sample, ["PC1", "PC2"]);
        assert_eq!(rows[0].resolved_at, None);
        // Rows of other sources are not this caller's.
        assert!(load(&pool, "other.").await.unwrap().is_empty());

        let later = t + chrono::Duration::minutes(1);
        let upd = d("nats_auth.x", "agent", 5, &["PC1"]);
        apply(&pool, &[Op::Update(upd)], later).await.unwrap();
        let rows = load(&pool, "nats_auth.").await.unwrap();
        assert_eq!((rows[0].count, rows[0].first_seen_at), (5, t));

        let end = t + chrono::Duration::minutes(2);
        apply(
            &pool,
            &[Op::Resolve {
                kind: "nats_auth.x".into(),
                subject: "agent".into(),
            }],
            end,
        )
        .await
        .unwrap();
        let rows = load(&pool, "nats_auth.").await.unwrap();
        assert_eq!(rows[0].resolved_at, Some(end));

        let again = t + chrono::Duration::minutes(9);
        apply(&pool, &[Op::Raise(first)], again).await.unwrap();
        let rows = load(&pool, "nats_auth.").await.unwrap();
        assert_eq!(rows.len(), 1, "same (kind, subject) row is reused");
        assert_eq!((rows[0].first_seen_at, rows[0].resolved_at), (again, None));
    }
}
