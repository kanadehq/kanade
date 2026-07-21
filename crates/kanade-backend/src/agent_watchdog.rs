//! Turns heartbeat gaps into durable `agent_offline` / `agent_online`
//! events.
//!
//! Heartbeats are core NATS with no history: a single `agents.last_heartbeat`
//! column, overwritten every 30 s. That is enough to say "this agent is not
//! reporting *right now*" and structurally incapable of saying "this agent
//! was not reporting last Tuesday" — so an outage, once it becomes history,
//! reverts to looking exactly like a machine that was quietly idle (#1089).
//!
//! obs_events, by contrast, are durable and carry their own `at`. Recording
//! the two transitions as events costs two rows per outage and makes the
//! outage permanently visible, instead of storing 2,880 "still alive"
//! messages per agent per day to reconstruct the same handful of edges.
//!
//! # What this deliberately will not claim
//!
//! Only outages observed *within a single backend run* are recorded. While
//! the backend is down nobody is watching heartbeats, so on restart every
//! agent looks stale — a naive sweep would announce a fleet-wide outage that
//! never happened. `observed_since` gates that: an agent whose last heartbeat
//! predates this process is one we have not seen report, and we say nothing
//! about it rather than guessing.
//!
//! The cost is real and accepted: an agent that drops while the backend is
//! restarting is never recorded. Reporting an outage that did not happen is
//! worse than missing one that did, and the whole point of these events is to
//! be trustworthy about what was observed.
use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use kanade_shared::{subject, wire::ObsEvent};
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};

use crate::api::agents::ALIVE_THRESHOLD;

/// `source` on the events this writes. Distinct from any `agent:*` scheme:
/// these are the backend's own inferences, not something a host reported, and
/// an operator reading the Events table should be able to tell the difference.
const SOURCE: &str = "backend:heartbeat-watchdog";

pub const KIND_OFFLINE: &str = "agent_offline";
pub const KIND_ONLINE: &str = "agent_online";

/// Per-agent state across sweeps.
#[derive(Debug, Clone, Copy)]
struct Outage {
    /// Last heartbeat before the gap — the `at` of the emitted offline event
    /// and the key that makes re-emission idempotent.
    since: DateTime<Utc>,
}

/// What a sweep concluded, before anything is published.
///
/// Split from the publishing so the rules — which agents are eligible, when
/// an outage opens, when it closes, when to stay quiet — can be tested
/// without NATS. Verifying them against a live broker would mean a second
/// backend sharing the production durable consumer, which would take messages
/// away from the real projector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Stopped reporting. `at` is the last heartbeat, the newest instant with
    /// evidence behind it.
    Offline { pc_id: String, at: DateTime<Utc> },
    /// Reporting again. `at` is the beat that proved it.
    Online {
        pc_id: String,
        at: DateTime<Utc>,
        since: DateTime<Utc>,
    },
}

impl Action {
    fn pc_id(&self) -> &str {
        match self {
            Action::Offline { pc_id, .. } | Action::Online { pc_id, .. } => pc_id,
        }
    }
}

/// Apply one sweep's outcomes to the open-outage map.
///
/// `outcomes` pairs each action with whether it was published. Pure, because
/// the interesting failure is not in the broker call but in what the state is
/// left as afterwards.
///
/// A sweep can emit two actions for one host — the close-then-reopen pair for
/// an agent that recovered and died again inside one interval. Handling those
/// independently loses data: if the `Online` publish fails and the `Offline`
/// then succeeds, inserting the new outage overwrites the entry that was
/// still waiting to be closed. The next sweep sees `outage.since == last`,
/// concludes it has already recorded this outage, and stays silent forever —
/// so the close is not retried, it is *dropped*, and the log ends up with the
/// gap-swallowing shape this feature exists to prevent.
///
/// So a host with any failed action this sweep is left entirely untouched.
/// `decide` regenerates the identical pair next time from the unchanged
/// state, and the two retry together.
fn apply_outcomes(
    open: &mut HashMap<String, Outage>,
    outcomes: &[(Action, bool)],
) -> (usize, usize) {
    let failed: std::collections::HashSet<&str> = outcomes
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(a, _)| a.pc_id())
        .collect();

    let mut went_offline = 0usize;
    let mut came_online = 0usize;
    for (action, ok) in outcomes {
        if !ok || failed.contains(action.pc_id()) {
            continue;
        }
        match action {
            Action::Offline { pc_id, at } => {
                open.insert(pc_id.clone(), Outage { since: *at });
                went_offline += 1;
            }
            Action::Online { pc_id, .. } => {
                open.remove(pc_id);
                came_online += 1;
            }
        }
    }
    (went_offline, came_online)
}

/// Pure decision step. `agents` is `(pc_id, last_heartbeat)`.
fn decide(
    agents: &[(String, DateTime<Utc>)],
    observed_since: DateTime<Utc>,
    open: &HashMap<String, Outage>,
    now: DateTime<Utc>,
) -> Vec<Action> {
    let cutoff = now - ALIVE_THRESHOLD;
    let mut out = Vec::new();
    for (pc_id, last) in agents {
        // Never observed reporting during this run — say nothing. Covers both
        // a backend restart (every agent looks stale until its next beat) and
        // an agent that has been dead since before we started.
        if *last < observed_since {
            continue;
        }
        if *last > cutoff {
            if let Some(outage) = open.get(pc_id) {
                out.push(Action::Online {
                    pc_id: pc_id.clone(),
                    at: *last,
                    since: outage.since,
                });
            }
            continue;
        }
        // Stale.
        if let Some(outage) = open.get(pc_id) {
            // Same outage as last sweep — already recorded, nothing to say.
            if outage.since == *last {
                continue;
            }
            // A NEWER heartbeat than the one that opened the outage, yet
            // already stale again: the agent recovered and died again inside
            // one sweep interval. We observed that beat, so the recovery is
            // evidence we hold and must not discard — closing the first
            // outage here is the only chance to record it. Skipping it would
            // leave two `agent_offline` events with no recovery between them,
            // and the strip would read the pair as one continuous outage,
            // swallowing an interval the host was demonstrably up.
            out.push(Action::Online {
                pc_id: pc_id.clone(),
                at: *last,
                since: outage.since,
            });
        }
        out.push(Action::Offline {
            pc_id: pc_id.clone(),
            at: *last,
        });
    }
    out
}

pub struct AgentWatchdog {
    /// When this process started watching. Nothing before it is asserted.
    ///
    /// Compared against `agents.last_heartbeat`, which is the agent's own
    /// `Utc::now()` at send time — so this gate straddles two clocks that
    /// nothing here synchronises. An agent running far enough fast can push a
    /// genuinely pre-restart beat past the gate, at which point a later
    /// silence produces a durable `agent_offline` for a window the backend
    /// never actually watched.
    ///
    /// The same mixed comparison already underlies `ALIVE_THRESHOLD` in
    /// `api/agents.rs` and the scheduler, but those are live flags that
    /// self-correct on the next beat; this is the first place it feeds a
    /// permanent record, so the assumption is worth naming: the fleet's
    /// clocks are expected to be roughly in step (NTP). Skew beyond a couple
    /// of minutes degrades what these events mean.
    observed_since: DateTime<Utc>,
    /// Agents currently believed offline, keyed by pc_id.
    open: HashMap<String, Outage>,
}

impl AgentWatchdog {
    pub fn new(observed_since: DateTime<Utc>) -> Self {
        Self {
            observed_since,
            open: HashMap::new(),
        }
    }

    /// One pass. Returns `(offline_emitted, online_emitted)`.
    ///
    /// Publishes to `obs.<pc_id>` so the events take the same durable path as
    /// an agent's own — they land in the same stream, are projected by the
    /// same consumer, and are subject to the same retention. The backend does
    /// not write `obs_events` rows directly; doing so here would create a
    /// second ingest path that the stream never sees.
    pub async fn sweep(
        &mut self,
        pool: &SqlitePool,
        js: &async_nats::jetstream::Context,
        now: DateTime<Utc>,
    ) -> Result<(usize, usize)> {
        let rows = sqlx::query(
            "SELECT pc_id, last_heartbeat FROM agents WHERE last_heartbeat IS NOT NULL",
        )
        .fetch_all(pool)
        .await?;

        let agents: Vec<(String, DateTime<Utc>)> = rows
            .iter()
            .filter_map(|r| {
                let pc_id: String = r.try_get("pc_id").ok()?;
                let last: DateTime<Utc> = r.try_get("last_heartbeat").ok()?;
                Some((pc_id, last))
            })
            .collect();

        // Publish first, collect what actually landed, then update state in
        // one pass. A host whose first action fails has its remaining actions
        // skipped rather than half-applied — see `apply_outcomes`.
        let mut outcomes: Vec<(Action, bool)> = Vec::new();
        let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();

        for action in decide(&agents, self.observed_since, &self.open, now) {
            if failed.contains(action.pc_id()) {
                // An earlier action for this host failed. Attempting this one
                // would record half a pair; leave the whole thing for the
                // next sweep, which regenerates it from unchanged state.
                outcomes.push((action, false));
                continue;
            }
            let published = match &action {
                Action::Offline { pc_id, at } => {
                    let r = self.publish(js, pc_id, KIND_OFFLINE, *at, *at).await;
                    match &r {
                        Ok(()) => info!(
                            pc_id = %pc_id,
                            last_heartbeat = %at,
                            threshold_secs = ALIVE_THRESHOLD.num_seconds(),
                            "agent stopped reporting; recorded agent_offline",
                        ),
                        Err(e) => {
                            warn!(pc_id = %pc_id, error = %e, "failed to publish agent_offline")
                        }
                    }
                    r.is_ok()
                }
                Action::Online { pc_id, at, since } => {
                    let r = self.publish(js, pc_id, KIND_ONLINE, *at, *since).await;
                    match &r {
                        Ok(()) => info!(
                            pc_id = %pc_id,
                            offline_since = %since,
                            back_at = %at,
                            "agent recovered; recorded agent_online",
                        ),
                        Err(e) => {
                            warn!(pc_id = %pc_id, error = %e, "failed to publish agent_online")
                        }
                    }
                    r.is_ok()
                }
            };
            if !published {
                failed.insert(action.pc_id().to_string());
            }
            outcomes.push((action, published));
        }

        let (went_offline, came_online) = apply_outcomes(&mut self.open, &outcomes);

        debug!(
            agents = agents.len(),
            went_offline, came_online, "agent watchdog sweep"
        );
        Ok((went_offline, came_online))
    }

    /// `at` is the instant the event describes; `key` distinguishes one
    /// outage from the next.
    ///
    /// `at` for an offline event is the **last heartbeat**, not that instant
    /// plus a heartbeat interval. A beat at T proves the agent was alive at
    /// T and nothing after it, so the unknown stretch opens at T. Padding
    /// forward would assert liveness across an interval nobody observed —
    /// and would need the agent's configured `heartbeat_interval`, which is
    /// per-PC and live-reloadable, to even compute.
    async fn publish(
        &self,
        js: &async_nats::jetstream::Context,
        pc_id: &str,
        kind: &str,
        at: DateTime<Utc>,
        key: DateTime<Utc>,
    ) -> Result<()> {
        // `event_record_id` must be Some and deterministic. The projector
        // dedups on UNIQUE(pc_id, source, event_record_id), and SQL NULL
        // never equals NULL — a None here would let every redelivery or
        // repeated sweep insert another row.
        let event = ObsEvent {
            pc_id: pc_id.to_string(),
            at,
            kind: kind.to_string(),
            source: SOURCE.to_string(),
            event_record_id: Some(format!("{kind}:{}", key.timestamp_millis())),
            payload: serde_json::json!({
                "last_heartbeat": key,
                "threshold_secs": ALIVE_THRESHOLD.num_seconds(),
            }),
        };
        let bytes = serde_json::to_vec(&event)?;
        let ack = js.publish(subject::obs(pc_id), bytes.into()).await?;
        ack.await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn offline_event_ids_are_deterministic_per_outage() {
        // Same outage → same id → the projector's UNIQUE drops the repeat.
        let a = format!("{KIND_OFFLINE}:{}", ts(0).timestamp_millis());
        let b = format!("{KIND_OFFLINE}:{}", ts(0).timestamp_millis());
        assert_eq!(a, b);
        // A later outage on the same host gets its own id.
        let c = format!("{KIND_OFFLINE}:{}", ts(600).timestamp_millis());
        assert_ne!(a, c);
    }

    #[test]
    fn online_and_offline_ids_do_not_collide() {
        // Both are keyed on the same instant when an outage is a single
        // sweep long; only the kind prefix separates them, so it must be
        // part of the id.
        let off = format!("{KIND_OFFLINE}:{}", ts(0).timestamp_millis());
        let on = format!("{KIND_ONLINE}:{}", ts(0).timestamp_millis());
        assert_ne!(off, on);
    }

    /// `observed_since` well before every fixture, so it is out of the way
    /// unless a test is specifically about it.
    const WATCH_START: i64 = 0;

    fn open_with(pc: &str, since: i64) -> HashMap<String, Outage> {
        HashMap::from([(pc.to_string(), Outage { since: ts(since) })])
    }

    /// `now` far enough past the heartbeat to be stale.
    fn stale_now(hb: i64) -> DateTime<Utc> {
        ts(hb) + ALIVE_THRESHOLD + chrono::Duration::seconds(1)
    }

    #[test]
    fn a_fresh_agent_produces_nothing() {
        let agents = vec![("pc1".into(), ts(100))];
        let out = decide(&agents, ts(WATCH_START), &HashMap::new(), ts(110));
        assert!(out.is_empty());
    }

    #[test]
    fn a_stale_agent_goes_offline_at_its_last_heartbeat() {
        let agents = vec![("pc1".into(), ts(100))];
        let out = decide(&agents, ts(WATCH_START), &HashMap::new(), stale_now(100));
        assert_eq!(
            out,
            vec![Action::Offline {
                pc_id: "pc1".into(),
                at: ts(100),
            }]
        );
    }

    #[test]
    fn an_already_recorded_outage_is_not_repeated() {
        // The sweep runs every 5 minutes; without this an agent down for an
        // hour would emit a dozen identical events. (The projector would
        // dedup them on the deterministic id, but re-publishing every tick
        // is still wasted work and noise in the log.)
        let agents = vec![("pc1".into(), ts(100))];
        let out = decide(
            &agents,
            ts(WATCH_START),
            &open_with("pc1", 100),
            stale_now(100),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn recovery_closes_the_outage() {
        let agents = vec![("pc1".into(), ts(900))];
        let out = decide(&agents, ts(WATCH_START), &open_with("pc1", 100), ts(905));
        assert_eq!(
            out,
            vec![Action::Online {
                pc_id: "pc1".into(),
                at: ts(900),
                since: ts(100),
            }]
        );
    }

    // Recovered and died again inside one sweep interval. The beat at 900 was
    // observed, so it has to be recorded — otherwise the log holds two
    // `agent_offline` events with no recovery between them and the strip
    // reads them as one continuous outage, swallowing an interval the host
    // was demonstrably up.
    #[test]
    fn a_recovery_missed_between_sweeps_is_still_recorded() {
        let agents = vec![("pc1".into(), ts(900))];
        let out = decide(
            &agents,
            ts(WATCH_START),
            &open_with("pc1", 100),
            stale_now(900),
        );
        assert_eq!(
            out,
            vec![
                Action::Online {
                    pc_id: "pc1".into(),
                    at: ts(900),
                    since: ts(100),
                },
                Action::Offline {
                    pc_id: "pc1".into(),
                    at: ts(900),
                },
            ],
            "the close must come first, and both must be emitted",
        );
    }

    // The property this whole module hinges on: while the backend was down
    // nobody watched heartbeats, so on restart every agent looks stale. A
    // sweep that ignored `observed_since` would announce a fleet-wide outage
    // that never happened.
    #[test]
    fn a_restart_does_not_invent_a_fleet_wide_outage() {
        let agents: Vec<(String, DateTime<Utc>)> =
            (0..50).map(|i| (format!("pc{i}"), ts(100))).collect();
        // Watching began after every one of those heartbeats.
        let out = decide(&agents, ts(500), &HashMap::new(), stale_now(500));
        assert!(
            out.is_empty(),
            "claimed {} outages for agents never observed reporting",
            out.len()
        );
    }

    #[test]
    fn an_agent_seen_after_the_watch_started_is_still_reported() {
        // The gate must not be so blunt that it silences everything after a
        // restart — an agent that reported and *then* went quiet is exactly
        // what we want to catch.
        let agents = vec![("pc1".into(), ts(600))];
        let out = decide(&agents, ts(500), &HashMap::new(), stale_now(600));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn the_threshold_boundary_is_not_stale() {
        // Exactly at the cutoff counts as reporting: `last > cutoff` is the
        // comparison, so a beat landing precisely on it must not flip.
        let agents = vec![("pc1".into(), ts(100))];
        let just_inside = ts(100) + ALIVE_THRESHOLD - chrono::Duration::seconds(1);
        assert!(decide(&agents, ts(WATCH_START), &HashMap::new(), just_inside).is_empty());
    }

    // ---- apply_outcomes: what a partial publish failure leaves behind ----

    fn off(pc: &str, at: i64) -> Action {
        Action::Offline {
            pc_id: pc.into(),
            at: ts(at),
        }
    }
    fn on(pc: &str, at: i64, since: i64) -> Action {
        Action::Online {
            pc_id: pc.into(),
            at: ts(at),
            since: ts(since),
        }
    }

    #[test]
    fn a_successful_pair_leaves_the_new_outage_open() {
        let mut open = open_with("pc1", 100);
        let out = apply_outcomes(
            &mut open,
            &[(on("pc1", 900, 100), true), (off("pc1", 900), true)],
        );
        assert_eq!(out, (1, 1));
        assert_eq!(open["pc1"].since, ts(900));
    }

    // The bug this guard exists for. A failed close followed by a successful
    // open used to overwrite the entry, so the next sweep saw
    // `outage.since == last`, decided the outage was already recorded, and
    // never retried the close — dropping it permanently and leaving the log
    // with the gap-swallowing shape the close was added to prevent.
    #[test]
    fn a_failed_close_does_not_let_the_reopen_bury_it() {
        let mut open = open_with("pc1", 100);
        let out = apply_outcomes(
            &mut open,
            &[(on("pc1", 900, 100), false), (off("pc1", 900), true)],
        );
        assert_eq!(out, (0, 0), "nothing may be counted as recorded");
        assert_eq!(
            open["pc1"].since,
            ts(100),
            "the outage still awaiting its close must survive untouched",
        );
        // And the next sweep must therefore regenerate the whole pair.
        let agents = vec![("pc1".into(), ts(900))];
        assert_eq!(
            decide(&agents, ts(WATCH_START), &open, stale_now(900)),
            vec![on("pc1", 900, 100), off("pc1", 900)],
        );
    }

    // Failure in the other order. The rule is deliberately blunt — any
    // failure for a host leaves that host's state entirely alone — rather
    // than "apply the ones that succeeded". Partial application is safe in
    // this direction and unsafe in the other, and depending on which is which
    // is precisely the subtlety that produced the bug this guard fixes.
    //
    // The cost is one redundant re-publish next sweep, which the deterministic
    // `event_record_id` makes a no-op at the projector.
    #[test]
    fn a_failed_open_leaves_the_host_untouched_and_replays_the_pair() {
        let mut open = open_with("pc1", 100);
        let out = apply_outcomes(
            &mut open,
            &[(on("pc1", 900, 100), true), (off("pc1", 900), false)],
        );
        assert_eq!(out, (0, 0));
        assert_eq!(open["pc1"].since, ts(100));

        let agents = vec![("pc1".into(), ts(900))];
        assert_eq!(
            decide(&agents, ts(WATCH_START), &open, stale_now(900)),
            vec![on("pc1", 900, 100), off("pc1", 900)],
            "the pair replays; the already-landed close dedups on its id",
        );
    }

    #[test]
    fn one_hosts_failure_does_not_hold_back_another() {
        let mut open = HashMap::new();
        let out = apply_outcomes(
            &mut open,
            &[(off("pc1", 100), false), (off("pc2", 100), true)],
        );
        assert_eq!(out, (1, 0));
        assert!(!open.contains_key("pc1"));
        assert_eq!(open["pc2"].since, ts(100));
    }

    #[test]
    fn hosts_are_judged_independently() {
        let agents = vec![
            ("fresh".into(), ts(1000)),
            ("stale".into(), ts(100)),
            ("unseen".into(), ts(-100)),
        ];
        // `now` chosen so `stale` is past the threshold while `fresh` is not.
        let out = decide(&agents, ts(0), &HashMap::new(), stale_now(100));
        assert_eq!(
            out,
            vec![Action::Offline {
                pc_id: "stale".into(),
                at: ts(100),
            }]
        );
    }
}
