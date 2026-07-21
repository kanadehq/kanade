//! Issue #246 — per-PC observability events.
//!
//! Distinct from [`super::EventStarted`] (lifecycle: "script just
//! spawned, watch this result_id"). `ObsEvent` is the timeline
//! data type: sign-in / sign-out, power on / off, sleep / resume,
//! agent self-update milestones — anything an operator wants to
//! see on a per-PC timeline. The actual log source is open-ended;
//! today it's Windows Event Log via a scheduled PowerShell job,
//! tomorrow it could be agent-emitted milestones or the
//! `kanade logs collect` diagnostic bundle (#219) carrying a
//! pointer to an Object Store blob.
//!
//! Schema is intentionally narrow: a stable triplet
//! (`pc_id`, `source`, `event_record_id`) for dedup-on-replay,
//! plus a `kind` enum-ish string for the SPA filter chip, plus a
//! free-form JSON `payload` for the per-kind details. Open-ended
//! by design — adding a new event source doesn't require a wire
//! change, just a new `kind` value.
//!
//! NATS subject: `obs.<pc_id>` (see [`super::super::subject::obs`]).
//! JetStream stream `OBS_EVENTS` retains ~30d so a backend that
//! was offline catches up on reconnect.
//!
//! De-dup is the BACKEND's job — agent re-sends are explicitly
//! allowed and useful (a watermark mismatch shouldn't lose data).
//! Backend UNIQUE constraint on
//! `(pc_id, source, event_record_id)` makes re-sends a no-op.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One observability event published to `obs.<pc_id>`.
///
/// The `kind` field is a free-form string — vocabulary lives at
/// the consumer (backend projector decides filtering / coloring;
/// SPA decides chip labels). Established kinds at #246 land:
///
/// - `logon` / `logoff` — Security log 4624 / 4634
/// - `boot` / `shutdown` — System log 12 / 13 (kernel-general)
/// - `unexpected_shutdown` — System log 41
/// - `sleep` / `resume` — System log 42 / 107
/// - `active` / `idle` — agent idle sampler (#841); the one lane
///   signal with no Event Log source
/// - `agent_update` — agent self-update milestone
/// - `agent_online` / `agent_offline` — whether the fleet was being
///   *observed*, as opposed to what a host was doing. Emitted from
///   two sides with different `source`s: the agent stamps
///   `agent:startup` at boot, and the backend infers both from
///   heartbeat gaps as `backend:heartbeat-watchdog` (#1107). They
///   drive no lane; they bound what the other lanes may claim.
/// - `diagnostic` — kanade logs collect bundles (#219)
///
/// New kinds can be added without a wire change; the backend
/// projector stores whatever string the agent sends and the SPA
/// surfaces it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ObsEvent {
    /// PC reporting the event. Routing key on the subject side
    /// (`obs.<pc_id>`) and primary scope of the SPA timeline view.
    pub pc_id: String,

    /// Wall-clock instant of the event as known to the SOURCE
    /// (e.g. Windows Event Log's `TimeCreated`), NOT the moment
    /// the agent published it. The timeline must reflect when
    /// things happened on the box, not when the projector heard
    /// about them — the two can differ by minutes when the agent
    /// is catching up from outbox after a broker outage.
    pub at: DateTime<Utc>,

    /// What kind of event this is — the SPA's filter chip and
    /// the backend projector's coloring key. See the doc comment
    /// on this struct for the vocabulary at landing.
    pub kind: String,

    /// Where this event came from. Format `<scheme>:<detail>`
    /// (e.g. `winlog:System`, `winlog:Security`, `agent:internal`,
    /// `kanade:logs_collect`). Two roles:
    ///
    /// - Distinguishes events from different sources that might
    ///   share an `event_record_id` namespace.
    /// - Lets the SPA filter "show me only winlog events" without
    ///   needing a separate enum.
    pub source: String,

    /// Stable per-source unique identifier — e.g. EventRecordID
    /// from the Windows Event Log. Combined with `pc_id` and
    /// `source` it forms the dedup key, so agent re-sends (under
    /// watermark drift, outbox replay, etc.) are harmless.
    ///
    /// `None` for sources that have no natural unique id (e.g.
    /// agent-emitted milestones where the only candidate is the
    /// `at` timestamp + kind, which the backend can synthesize
    /// from those fields if needed).
    ///
    /// `#[serde(default)]` so an agent publisher that has no id
    /// to emit can omit the field entirely; serde fills `None`
    /// rather than refusing the message. Without this, agent
    /// versions that always send the field can deserialize but
    /// future ones that drop it on `null` cases would silently
    /// land in the warn-log → projector drop path.
    #[serde(default)]
    pub event_record_id: Option<String>,

    /// Free-form per-kind details. The wire stays narrow
    /// (`pc_id`, `at`, `kind`, `source`, `event_record_id`) and
    /// the per-kind shape lives here:
    ///
    /// - `logon`: `{ "user": "...", "logon_type": 2 }`
    /// - `boot`: typically `null` or `{}` — the bare presence is
    ///   the event
    /// - `diagnostic`: `{ "bucket": "OBJECT_DIAGNOSTICS", "key":
    ///   "..." }` — pointer to the actual log blob
    ///
    /// Backend projector stores this as TEXT (the JSON
    /// representation); SPA renders it kind-aware.
    ///
    /// `#[serde(default)]` so a publisher emitting a bare-presence
    /// event can omit the field entirely (serde fills
    /// `Value::Null`) rather than being forced to write
    /// `"payload": null` on every line.
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    #[test]
    fn obs_event_round_trips_through_json() {
        let t = Utc.with_ymd_and_hms(2026, 5, 28, 10, 41, 0).unwrap();
        let e = ObsEvent {
            pc_id: "pc-01".into(),
            at: t,
            kind: "logon".into(),
            source: "winlog:Security".into(),
            event_record_id: Some("1234567".into()),
            payload: json!({ "user": "yukimemi", "logon_type": 2 }),
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: ObsEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn obs_event_null_payload_is_valid() {
        // boot / shutdown / similar bare-presence events: the
        // `at` + `kind` is the whole signal, payload carries
        // nothing. Make sure null serialises round-trip clean
        // so the agent's PowerShell side can emit an explicit
        // `null` without backend deserialise rejecting.
        let e = ObsEvent {
            pc_id: "pc-01".into(),
            at: Utc.with_ymd_and_hms(2026, 5, 28, 0, 0, 0).unwrap(),
            kind: "boot".into(),
            source: "winlog:System".into(),
            event_record_id: Some("99".into()),
            payload: serde_json::Value::Null,
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: ObsEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn obs_event_missing_event_record_id_deserialises() {
        // Agent-emitted milestones (e.g. agent_started) have no
        // natural EventRecordID equivalent. The field is
        // `Option<String>` annotated `#[serde(default)]`, so a
        // publisher omitting it entirely lands as `None` rather
        // than failing the deserialise. Gemini #247 HIGH —
        // without the explicit default, serde requires Option
        // fields to be present (allowed to be null, not absent).
        let s = r#"{
            "pc_id": "pc-01",
            "at": "2026-05-28T10:00:00Z",
            "kind": "agent_started",
            "source": "agent:internal",
            "payload": null
        }"#;
        let e: ObsEvent = serde_json::from_str(s).unwrap();
        assert_eq!(e.event_record_id, None);
        assert_eq!(e.kind, "agent_started");
    }

    #[test]
    fn obs_event_missing_payload_defaults_to_null() {
        // Bare-presence events (boot / shutdown / agent_started)
        // have no per-kind details. A publisher that omits the
        // `payload` field entirely should parse — `#[serde(default)]`
        // on the field fills `Value::Null` so the projector's
        // `INSERT` sees the same value as if the publisher had
        // written `"payload": null` explicitly.
        let s = r#"{
            "pc_id": "pc-01",
            "at": "2026-05-28T10:00:00Z",
            "kind": "boot",
            "source": "winlog:System",
            "event_record_id": "1"
        }"#;
        let e: ObsEvent = serde_json::from_str(s).unwrap();
        assert_eq!(e.payload, serde_json::Value::Null);
    }
}
