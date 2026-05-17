use serde::{Deserialize, Serialize};

/// Liveness ping every agent sends on a 30 s cadence (see
/// `inventory_interval` / `heartbeat_interval` in agent_config).
///
/// `hostname` and `os_family` are enriched baseline facts so the
/// SPA agents page has *something* to show as soon as the agent
/// boots — even when the full WMI-driven `HwInventory` hasn't been
/// (or can't be) collected. Both stay `Option<String>` so older
/// agents that don't send them still deserialize cleanly.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Heartbeat {
    pub pc_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub agent_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Coarse OS bucket from `std::env::consts::OS` — `"windows"`,
    /// `"linux"`, `"macos"`. Rich OS metadata still flows through
    /// the inventory path; this is just the "agent is alive on a
    /// <family>" signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_family: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn heartbeat_round_trips_through_json() {
        let hb = Heartbeat {
            pc_id: "minipc".into(),
            at: chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap(),
            agent_version: "0.12.0".into(),
            hostname: Some("MINIPC".into()),
            os_family: Some("windows".into()),
        };
        let json = serde_json::to_string(&hb).unwrap();
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pc_id, hb.pc_id);
        assert_eq!(back.at, hb.at);
        assert_eq!(back.agent_version, hb.agent_version);
        assert_eq!(back.hostname, hb.hostname);
        assert_eq!(back.os_family, hb.os_family);
    }

    #[test]
    fn heartbeat_without_enrichment_still_decodes() {
        // Older agents sending only the v0.11 shape must still parse.
        let json = r#"{"pc_id":"x","at":"2026-05-16T00:00:00Z","agent_version":"0.11.5"}"#;
        let hb: Heartbeat = serde_json::from_str(json).unwrap();
        assert_eq!(hb.pc_id, "x");
        assert_eq!(hb.hostname, None);
        assert_eq!(hb.os_family, None);
    }
}
