//! Records "this agent started" as a durable timeline event.
//!
//! The backend already infers outages from heartbeat gaps (#1107), which is
//! enough to mark a stretch as unknown but leaves three things it cannot know
//! from the outside:
//!
//! - **When the agent actually came back.** The backend closes an outage at
//!   whatever `last_heartbeat` reads when its sweep runs — the *newest* beat,
//!   not the first one after recovery. On a 5-minute sweep that overstates the
//!   outage by up to a sweep interval. This event carries the real instant.
//!
//! - **Restarts shorter than the staleness threshold.** Come back inside two
//!   minutes and the heartbeat never looks stale, so no sweep ever notices and
//!   no outage is recorded at all — yet the agent was down, and the strip goes
//!   on claiming that window as known. Self-updates land here routinely.
//!
//! - **Restarts during the backend's own downtime.** `observed_since` stops
//!   the backend asserting anything about a window it was not watching, which
//!   is correct and also permanent: it can never fill that gap in. The agent
//!   can — this goes through the obs outbox, so it survives the broker being
//!   unreachable and backfills with its true `at` once both are up.
//!
//! Deliberately startup-only. A matching shutdown event would only ever cover
//! *graceful* stops, so the backend's inference stays necessary regardless,
//! and hooking the shutdown path risks delaying the SCM stop transition, which
//! is already on a bounded teardown budget (#500).
use tracing::{info, warn};

use crate::obs_outbox;

/// Enqueue an `agent_online` event stamped now.
///
/// Best-effort, like the self-update milestone: a failure to record that we
/// started must never stop us from starting.
pub fn emit(pc_id: &str, agent_version: &str, obs_outbox_dir: &std::path::Path) {
    let event = kanade_shared::wire::ObsEvent {
        pc_id: pc_id.to_string(),
        at: chrono::Utc::now(),
        kind: "agent_online".to_string(),
        // Distinct from the backend's `backend:heartbeat-watchdog`, which
        // emits the same kind by inference. Both can describe one recovery —
        // this one with the true instant, that one with whenever its sweep
        // looked — and the source is what tells them apart, in the Events
        // table and in the projector's UNIQUE(pc_id, source, event_record_id).
        source: "agent:startup".to_string(),
        // A UUID rather than the timestamp: every start is a real, distinct
        // event, so nothing here should ever dedup. (A timestamp key would
        // also be fine in practice, but it invites the reading that repeats
        // are meant to collapse, which is true of the backend's inferred
        // events and false of these.)
        event_record_id: Some(format!("startup_{}", uuid::Uuid::new_v4().simple())),
        // `boot_time` is what lets the backend name the CAUSE of the outage
        // this start ends, without a periodic sentinel write or any Event Log
        // parsing (#1316). Compared against the last heartbeat before the
        // gap:
        //
        //   boot after  the last beat → the host rebooted while we were gone
        //                               → the MACHINE was away
        //   boot before the last beat → the machine never rebooted, so it was
        //                               up the whole time → the AGENT was
        //                               stopped (crash, service stop, a
        //                               self-update that did not come back)
        //
        // The third case — only the link was gone — is not decided here and
        // is deliberately NOT "an outage that closes without one of these".
        // A link outage does not restart the agent, so the absence of a
        // startup event is consistent with it; it is also consistent with an
        // agent too old to send a boot time, and with one whose startup event
        // is still sitting in the outbox. Answering from the absence would
        // report the most reassuring of the three causes on the strength of
        // nothing. What identifies it is positive evidence — the agent's own
        // records timestamped inside the silent stretch, arriving afterwards
        // — and that lives in the consumer (`outageReason`), because it is a
        // judgement about a whole window rather than about this one event.
        //
        // Epoch seconds, and derived (`now − uptime`), so it jitters by a few
        // seconds across one boot session as the clock is disciplined — see
        // `local_scheduler::STARTUP_BOOT_THRESHOLD_SECS`, which absorbs the
        // same jitter for `on: startup`. Two genuinely different boots are
        // always minutes apart, so no tolerance is needed to tell them apart;
        // it matters only for comparisons against a nearby instant.
        //
        // `boot_time()` answers 0 when the platform cannot say. Sent as null
        // rather than 0 so a consumer cannot read the epoch as "booted in
        // 1970" and conclude the machine has been up for 56 years.
        payload: serde_json::json!({
            "agent_version": agent_version,
            "boot_time": match sysinfo::System::boot_time() {
                0 => None,
                secs => Some(secs),
            },
        }),
    };
    let res = obs_outbox::ensure_outbox_dir(obs_outbox_dir)
        .and_then(|()| obs_outbox::enqueue(obs_outbox_dir, &event).map(|_| ()));
    match res {
        Ok(()) => info!(agent_version, "queued agent_online obs event"),
        Err(e) => warn!(error = %e, "failed to queue agent_online obs event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueues_a_file_the_drain_can_ship() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obs-outbox");
        emit("pc1", "0.44.29", &path);

        let files: Vec<_> = std::fs::read_dir(&path)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .collect();
        assert_eq!(files.len(), 1, "one queued event");

        let raw = std::fs::read(files[0].path()).unwrap();
        let ev: kanade_shared::wire::ObsEvent = serde_json::from_slice(&raw).unwrap();
        assert_eq!(ev.kind, "agent_online");
        assert_eq!(ev.source, "agent:startup");
        assert_eq!(ev.pc_id, "pc1");
        assert_eq!(ev.payload["agent_version"], "0.44.29");
        assert!(
            ev.event_record_id.is_some(),
            "must not be None: NULL never dedups, but more importantly every start is its own event"
        );
    }

    // Two starts are two events. The backend's inferred `agent_online` dedups
    // on a shared timestamp key by design; these must not, or a restart loop
    // would collapse into a single recorded start.
    #[test]
    fn two_starts_are_two_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obs-outbox");
        emit("pc1", "0.44.29", &path);
        emit("pc1", "0.44.29", &path);

        let n = std::fs::read_dir(&path)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count();
        assert_eq!(n, 2);
    }

    // Recording that we started must never be able to stop us starting.
    #[test]
    fn an_unusable_directory_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        // A *file* where the outbox directory should be: creation fails.
        let path = dir.path().join("obs-outbox");
        std::fs::write(&path, b"not a directory").unwrap();
        emit("pc1", "0.44.29", &path);
    }
}
