//! Agent-native idle / active presence sampler (#841).
//!
//! Of the operational signals on the swimlane (power / session / sleep /
//! active), every one EXCEPT active-vs-idle is recorded in the Windows Event
//! Log and can be read back gap-free after the fact. "Is an interactive user
//! actually using the box?" is the exception: it's a live measurement
//! (`GetLastInputInfo` via WTS, see [`crate::env_gate::console_idle`]) with no
//! log to scrape later. So unlike the winlog-sourced lanes, this one must be
//! sampled by the resident agent.
//!
//! We emit on the **debounced transition** only, not every tick:
//!   - `active` — interactive input resumed (idle < [`IDLE_THRESHOLD`])
//!   - `idle`   — no input for ≥ [`IDLE_THRESHOLD`], or no console user at all
//!
//! so a day of work is a handful of events (≈ logon/sleep volume), not hundreds
//! of samples — and it folds straight into the swimlane's `active` lane (a
//! filled span = `active` → `idle`). The events go to the same `obs_outbox` the
//! winlog/self-update producers use; the drain publishes them.
//!
//! #841 will make the threshold operator-tunable via a `collector` resource;
//! for now it's a built-in default (batteries-included).

use std::path::{Path, PathBuf};
use std::time::Duration;

use kanade_shared::wire::ObsEvent;
use tracing::warn;

/// How often we read the idle clock. 30 s matches the KLP evaluator cadence
/// and bounds transition latency; the read itself (one WTS query) is cheap.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Idle for ≥ this long counts as "idle". 5 min is long enough that a pause to
/// read or think doesn't flip the lane, short enough that a real break shows.
const IDLE_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// `obs_events.source` for the sampler — `<scheme>:<detail>`, matching the
/// `agent:self_update` convention for agent-native producers.
const SOURCE: &str = "agent:idle_sampler";

/// Whether an interactive user is actively using the console.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Presence {
    Active,
    Idle,
}

impl Presence {
    fn kind(self) -> &'static str {
        match self {
            Presence::Active => "active",
            Presence::Idle => "idle",
        }
    }
}

/// Classify one idle reading. `None` (idle couldn't be read) → `None`, so a bad
/// sample never flips the lane. A headless console reports `Duration::MAX`,
/// which is ≥ threshold → `Idle` — a logged-out box stays idle and never churns
/// active/idle events.
fn classify(idle: Option<Duration>, threshold: Duration) -> Option<Presence> {
    match idle {
        Some(d) if d >= threshold => Some(Presence::Idle),
        Some(_) => Some(Presence::Active),
        None => None,
    }
}

/// Fold a reading into the running state; return `Some(p)` to emit when it's a
/// change (including the first known reading, which anchors the lane at
/// agent-start). An unknown reading (`None`) leaves the state untouched.
fn step(state: &mut Option<Presence>, reading: Option<Presence>) -> Option<Presence> {
    let r = reading?;
    if *state == Some(r) {
        return None;
    }
    *state = Some(r);
    Some(r)
}

/// One idle reading from the OS, `None` when it can't be determined. Windows
/// reads the console idle clock; other targets (CI / dev — no production agents)
/// report `None`, so the sampler simply never emits there.
#[cfg(target_os = "windows")]
fn sample_idle() -> Option<Duration> {
    crate::env_gate::console_idle()
}

#[cfg(not(target_os = "windows"))]
fn sample_idle() -> Option<Duration> {
    None
}

/// Build the `obs_event` for a presence transition. `event_record_id` is the
/// kind + the transition instant (ms), a stable per-transition key so an
/// outbox/broker redelivery dedups against the backend
/// `UNIQUE(pc_id, source, event_record_id)`.
fn build_event(pc_id: &str, p: Presence, at: chrono::DateTime<chrono::Utc>) -> ObsEvent {
    ObsEvent {
        pc_id: pc_id.to_string(),
        at,
        kind: p.kind().to_string(),
        source: SOURCE.to_string(),
        event_record_id: Some(format!("{}:{}", p.kind(), at.timestamp_millis())),
        payload: serde_json::Value::Null,
    }
}

/// Persist a transition event to the outbox (best-effort — the drain publishes
/// it). A failed enqueue is logged and dropped: idle/active is ambient signal,
/// not worth retrying inline.
fn emit(pc_id: &str, dir: &Path, p: Presence) {
    let event = build_event(pc_id, p, chrono::Utc::now());
    if let Err(e) = crate::obs_outbox::enqueue(dir, &event) {
        warn!(error = %e, kind = p.kind(), "idle_sampler: enqueue failed");
    }
}

/// Long-lived sampler: read the idle clock every [`SAMPLE_INTERVAL`] and emit
/// an `active` / `idle` event on each debounced transition. Spawned once at
/// agent start; runs for the process lifetime.
pub async fn run(pc_id: String, obs_outbox_dir: PathBuf) {
    if let Err(e) = crate::obs_outbox::ensure_outbox_dir(&obs_outbox_dir) {
        warn!(error = %e, "idle_sampler: outbox dir — events may be dropped until it exists");
    }
    let mut state: Option<Presence> = None;
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    // Skip (not Burst, the default): across a suspend/resume the missed ticks
    // would otherwise fire back-to-back to "catch up" — pointless for a
    // periodic sampler. One tick on resume, then the normal cadence.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let reading = classify(sample_idle(), IDLE_THRESHOLD);
        if let Some(p) = step(&mut state, reading) {
            emit(&pc_id, &obs_outbox_dir, p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESH: Duration = Duration::from_secs(300);

    #[test]
    fn classify_splits_on_threshold() {
        assert_eq!(
            classify(Some(Duration::ZERO), THRESH),
            Some(Presence::Active)
        );
        assert_eq!(
            classify(Some(Duration::from_secs(299)), THRESH),
            Some(Presence::Active)
        );
        // Exactly the threshold counts as idle (≥).
        assert_eq!(classify(Some(THRESH), THRESH), Some(Presence::Idle));
        assert_eq!(
            classify(Some(Duration::from_secs(3600)), THRESH),
            Some(Presence::Idle)
        );
        // Headless / no console user → idle.
        assert_eq!(classify(Some(Duration::MAX), THRESH), Some(Presence::Idle));
        // Unreadable → no opinion.
        assert_eq!(classify(None, THRESH), None);
    }

    #[test]
    fn step_emits_baseline_then_only_on_change() {
        let mut state = None;
        // First known reading anchors the lane (emits), establishing baseline.
        assert_eq!(
            step(&mut state, Some(Presence::Active)),
            Some(Presence::Active)
        );
        // Same state again → no emit.
        assert_eq!(step(&mut state, Some(Presence::Active)), None);
        // Transition → emit.
        assert_eq!(step(&mut state, Some(Presence::Idle)), Some(Presence::Idle));
        assert_eq!(step(&mut state, Some(Presence::Idle)), None);
        // Back to active → emit.
        assert_eq!(
            step(&mut state, Some(Presence::Active)),
            Some(Presence::Active)
        );
    }

    #[test]
    fn step_ignores_unknown_readings() {
        let mut state = Some(Presence::Active);
        // An unreadable sample must not flip or clear the state.
        assert_eq!(step(&mut state, None), None);
        assert_eq!(state, Some(Presence::Active));
    }

    #[test]
    fn unknown_reading_before_baseline_stays_unset() {
        let mut state = None;
        assert_eq!(step(&mut state, None), None);
        assert_eq!(state, None);
        // The first *known* reading still emits as the baseline afterwards.
        assert_eq!(step(&mut state, Some(Presence::Idle)), Some(Presence::Idle));
    }

    #[test]
    fn build_event_has_stable_dedup_key_and_null_payload() {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let e = build_event("PC-01", Presence::Idle, at);
        assert_eq!(e.pc_id, "PC-01");
        assert_eq!(e.kind, "idle");
        assert_eq!(e.source, "agent:idle_sampler");
        assert_eq!(e.event_record_id.as_deref(), Some("idle:1700000000000"));
        assert_eq!(e.payload, serde_json::Value::Null);
        // active at the same instant has a distinct key (kind-prefixed).
        let a = build_event("PC-01", Presence::Active, at);
        assert_ne!(a.event_record_id, e.event_record_id);
    }
}
