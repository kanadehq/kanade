//! `maintenance.*` method types — scheduled-job preview + reboot
//! defer.
//!
//! Per SPEC §2.1: the Client App shows the user "what's about to
//! happen on my PC" (next N days of scheduled jobs targeting this
//! PC, derived from `BUCKET_SCHEDULES` ∩ this agent's groups +
//! pc_id) and lets them push back on imminent restarts via
//! `maintenance.defer` (15m / 30m / 1h, per SPEC §2.1).

use serde::{Deserialize, Serialize};

// ---------- maintenance.list ----------

/// `maintenance.list` params — preview window in days.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct MaintenanceListParams {
    /// How far into the future to preview. Clamped agent-side to
    /// [1, 30]. Defaults to 7 days.
    #[serde(default = "default_window_days")]
    pub window_days: u32,
}

impl Default for MaintenanceListParams {
    fn default() -> Self {
        Self {
            window_days: default_window_days(),
        }
    }
}

fn default_window_days() -> u32 {
    7
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct MaintenanceListResult {
    pub items: Vec<MaintenanceItem>,
}

/// One upcoming scheduled job that will fire against this PC.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct MaintenanceItem {
    /// Schedule id from `BUCKET_SCHEDULES`.
    pub schedule_id: String,
    /// Manifest id the schedule fires (matches everywhere else —
    /// `Schedule.job_id`, `Manifest.id`).
    pub manifest_id: String,
    /// Manifest's `display_name` (or `Manifest.id` if no display
    /// name is set) so the Client App doesn't need a second lookup.
    pub display_name: String,
    /// Next absolute time this schedule will fire at this PC,
    /// computed from the schedule's cron expression. UTC.
    pub next_fire_at: chrono::DateTime<chrono::Utc>,
    /// `true` if this is the schedule's *deferrable* run — currently
    /// only true for reboot manifests with `category:
    /// software_update`. SPA enables the "延期申請" button when
    /// true.
    #[serde(default)]
    pub deferrable: bool,
}

// ---------- maintenance.defer ----------

/// `maintenance.defer` params — push back a scheduled reboot. The
/// agent records the deferral and skips the next fire of the named
/// schedule for the chosen window (SPEC §2.1: 15m / 30m / 1h).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct MaintenanceDeferParams {
    pub schedule_id: String,
    pub duration: DeferDuration,
}

/// Allowed defer windows per SPEC §2.1 (`15分 / 30分 / 1時間`). The
/// fixed set avoids operators having to think about long-tail "user
/// deferred 3 days" scenarios — anything bigger goes through the
/// helpdesk.
///
/// Wire form mirrors SPEC §2.1's `15m / 30m / 1h` humantime
/// shorthand verbatim so JSON payloads read like operator-spoken
/// shorthand. The `#[non_exhaustive]` annotation leaves room for
/// future SPEC bumps to add windows (e.g. `2h`) without forcing a
/// wire-protocol version change — downstream Rust consumers see a
/// compile-time nudge to add a wildcard arm.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeferDuration {
    /// 15 minutes.
    #[serde(rename = "15m")]
    M15,
    /// 30 minutes.
    #[serde(rename = "30m")]
    M30,
    /// 1 hour.
    #[serde(rename = "1h")]
    H1,
}

impl DeferDuration {
    /// Wall-clock duration this enum variant represents.
    pub fn as_duration(self) -> chrono::Duration {
        match self {
            Self::M15 => chrono::Duration::minutes(15),
            Self::M30 => chrono::Duration::minutes(30),
            Self::H1 => chrono::Duration::hours(1),
        }
    }
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct MaintenanceDeferResult {
    /// Absolute time the schedule will fire (next), after applying
    /// the defer. Lets the SPA show "Deferred to 14:30" without
    /// re-querying `maintenance.list`.
    pub new_fire_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn params_default_window_is_seven_days() {
        let p = MaintenanceListParams::default();
        assert_eq!(p.window_days, 7);
        let p: MaintenanceListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.window_days, 7);
    }

    #[test]
    fn item_with_deferrable_round_trips() {
        let t = chrono::Utc
            .with_ymd_and_hms(2026, 5, 25, 14, 30, 0)
            .unwrap();
        let i = MaintenanceItem {
            schedule_id: "weekly-reboot".into(),
            manifest_id: "reboot".into(),
            display_name: "再起動".into(),
            next_fire_at: t,
            deferrable: true,
        };
        let json = serde_json::to_string(&i).unwrap();
        let back: MaintenanceItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schedule_id, i.schedule_id);
        assert_eq!(back.display_name, "再起動");
        assert_eq!(back.next_fire_at, t);
        assert!(back.deferrable);
    }

    #[test]
    fn defer_duration_wire_matches_spec_2_1_humantime() {
        // SPEC §2.1 writes the windows as `15分 / 30分 / 1時間`,
        // and the documented operator shorthand is humantime
        // (`15m / 30m / 1h`). Pin the wire so a future enum rename
        // can't silently drift the Client App's "延期" button
        // payloads.
        for (variant, expected) in [
            (DeferDuration::M15, "\"15m\""),
            (DeferDuration::M30, "\"30m\""),
            (DeferDuration::H1, "\"1h\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "encode {variant:?}");
            let back: DeferDuration = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant, "round-trip {expected}");
        }
    }

    #[test]
    fn defer_duration_as_duration_matches_spec_table() {
        assert_eq!(
            DeferDuration::M15.as_duration(),
            chrono::Duration::minutes(15)
        );
        assert_eq!(
            DeferDuration::M30.as_duration(),
            chrono::Duration::minutes(30)
        );
        assert_eq!(DeferDuration::H1.as_duration(), chrono::Duration::hours(1));
    }

    #[test]
    fn defer_result_round_trips() {
        let t = chrono::Utc.with_ymd_and_hms(2026, 5, 24, 15, 0, 0).unwrap();
        let r = MaintenanceDeferResult { new_fire_at: t };
        let json = serde_json::to_string(&r).unwrap();
        let back: MaintenanceDeferResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.new_fire_at, t);
    }

    #[test]
    fn item_deferrable_defaults_to_false() {
        let wire = r#"{
            "schedule_id":"x","manifest_id":"y","display_name":"z",
            "next_fire_at":"2026-05-24T00:00:00Z"
        }"#;
        let i: MaintenanceItem = serde_json::from_str(wire).unwrap();
        assert!(!i.deferrable);
    }
}
