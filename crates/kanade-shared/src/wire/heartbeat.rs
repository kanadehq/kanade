use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Heartbeat {
    pub pc_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub agent_version: String,
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
            agent_version: "0.1.4".into(),
        };
        let json = serde_json::to_string(&hb).unwrap();
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pc_id, hb.pc_id);
        assert_eq!(back.at, hb.at);
        assert_eq!(back.agent_version, hb.agent_version);
    }
}
