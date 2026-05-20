//! v0.30 / PR α' — agent-emitted lifecycle event published just
//! before script spawn. Lets the backend create an `execution_results`
//! row with `finished_at = NULL` so the SPA Activity table can show
//! in-flight runs alongside finished ones (filterable via status).
//!
//! Why this exists separately from [`super::ExecResult`]: an
//! ExecResult only lands when the script returns, which can be many
//! minutes (or never, for stuck processes). EventStarted closes the
//! "what's running NOW" gap without overloading ExecResult's "what
//! came out" semantics.
//!
//! `result_id` is the same UUID the agent will later use on the
//! matching ExecResult. The agent mints it in `handle_command`
//! before publishing events.started, then threads it through to the
//! ExecResult builder so both end up writing to the same
//! `execution_results.result_id` row. Backend UPSERTs against this
//! key — events.started INSERTs (or ON CONFLICT no-op), ExecResult
//! INSERTs-or-UPDATEs in one shot.
//!
//! Offline handling: published via the file-based outbox same as
//! ExecResult (see `kanade-agent::events_outbox`). Survives broker
//! outages + agent restarts; backend sees `started → finished` as a
//! coherent pair once the agent reconnects and both outboxes drain.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventStarted {
    /// Agent-minted UUID, identical to the `ExecResult.result_id`
    /// that will follow when the script finishes. Backend uses it
    /// as the UPSERT key so events.started and the matching
    /// ExecResult coalesce into a single `execution_results` row
    /// regardless of arrival order.
    pub result_id: String,
    /// NATS reply token for this run. Mirrors
    /// [`super::ExecResult::request_id`]. Carried here so the
    /// events projector can populate `execution_results.request_id`
    /// (NOT NULL in the schema) at insert time without waiting for
    /// the matching ExecResult.
    pub request_id: String,
    /// Deployment / scheduler-fire UUID. Same value as
    /// [`super::Command::exec_id`].
    pub exec_id: String,
    /// PC reporting the start.
    pub pc_id: String,
    /// Wall-clock instant the agent took just before
    /// `tokio::process::Command::spawn()`. Same value will end up
    /// on the matching `ExecResult.started_at`.
    pub started_at: DateTime<Utc>,
    /// `Manifest.id` for the running script. Useful for the SPA
    /// Activity table so each running row knows what's running
    /// without a lookup.
    pub manifest_id: String,
    /// Pinned manifest version. Same field as `Command.version`.
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
            result_id: "result-uuid-1".into(),
            request_id: "req-1".into(),
            exec_id: "exec-uuid-1".into(),
            pc_id: "minipc".into(),
            started_at: t,
            manifest_id: "inventory-hw".into(),
            version: "1.0.0".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: EventStarted = serde_json::from_str(&json).unwrap();
        assert_eq!(back.result_id, e.result_id);
        assert_eq!(back.request_id, e.request_id);
        assert_eq!(back.exec_id, e.exec_id);
        assert_eq!(back.pc_id, e.pc_id);
        assert_eq!(back.started_at, t);
        assert_eq!(back.manifest_id, e.manifest_id);
        assert_eq!(back.version, e.version);
    }
}
