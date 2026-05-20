//! v0.30 / PR α — agent-emitted lifecycle events that aren't
//! [`ExecResult`]s. ExecResult covers "the script finished, here's
//! the outcome". `EventStarted` covers "I'm about to start the
//! script" so the backend can show in-flight per-PC rows on the
//! Activity Running tab before any result lands.
//!
//! Subject convention is `events.started.{exec_id}.{pc_id}` —
//! per-pair so a backend filter wildcard can pick up only the
//! lifecycle subset (`events.>`), and a future per-deployment
//! consumer could narrow further. The payload itself also carries
//! `exec_id` + `pc_id` so the projector doesn't have to parse the
//! subject token.
//!
//! Why a separate wire type instead of overloading ExecResult with
//! a "started but not finished" variant: a started event has no
//! exit_code / stdout / stderr (the script hasn't produced any yet)
//! and Option-ing those fields out of ExecResult would make the
//! projector's existing INSERT path branchier than just having two
//! distinct shapes. The `running_runs` table the backend projector
//! writes to is also distinct from `execution_results` — different
//! lifecycle, different PK, different cleanup rules.
//!
//! Offline considerations: the agent persists `EventStarted` to its
//! file outbox before publishing (same v0.24 mechanism that gives
//! `ExecResult` durability across broker outages). On reconnect the
//! drain loop replays both in produce order, so backend sees
//! `started → finished` as a coherent pair even when an agent was
//! disconnected for hours mid-run.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventStarted {
    /// The deployment / scheduler-fire UUID this run belongs to.
    /// Same value as `Command.exec_id` — the agent copies it through
    /// at script-spawn time.
    pub exec_id: String,
    /// PC reporting the start. Mirrors `ExecResult.pc_id`.
    pub pc_id: String,
    /// Wall-clock instant the agent took just before
    /// `tokio::process::Command::spawn()`. The same value will end
    /// up on the matching `ExecResult.started_at` once the script
    /// finishes — so backend can correlate per-PC.
    pub started_at: DateTime<Utc>,
    /// `Manifest.id` for the running script. Useful for the SPA
    /// Running tab so each row knows what's running without an
    /// extra `/api/jobs/{exec_id}/...` lookup.
    pub manifest_id: String,
    /// The Manifest version pinned to the live publish. Same field
    /// as `Command.version`.
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn event_started_round_trips_through_json() {
        let t = Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap();
        let e = EventStarted {
            exec_id: "exec-uuid-1".into(),
            pc_id: "minipc".into(),
            started_at: t,
            manifest_id: "inventory-hw".into(),
            version: "1.0.0".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: EventStarted = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exec_id, e.exec_id);
        assert_eq!(back.pc_id, e.pc_id);
        assert_eq!(back.started_at, t);
        assert_eq!(back.manifest_id, e.manifest_id);
        assert_eq!(back.version, e.version);
    }
}
