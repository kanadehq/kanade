use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecResult {
    pub request_id: String,
    pub pc_id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    /// v0.13: the manifest id that produced this result. Sourced
    /// from `Command.id` (which is the YAML `manifest.id`, e.g.
    /// `"inventory-hw"`). Distinct from the per-deploy UUID stored
    /// in `Command.job_id`. The results projector uses this to
    /// look up the manifest's `inventory:` hint and upsert
    /// `inventory_facts` rows for inventory-tagged jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn exec_result_round_trips_through_json() {
        let t0 = chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap();
        let t1 = chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 5).unwrap();
        let r = ExecResult {
            request_id: "req-1".into(),
            pc_id: "minipc".into(),
            exit_code: 0,
            stdout: "hello\n".into(),
            stderr: String::new(),
            started_at: t0,
            finished_at: t1,
            manifest_id: Some("inventory-hw".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: ExecResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, r.request_id);
        assert_eq!(back.exit_code, r.exit_code);
        assert_eq!(back.stdout, r.stdout);
        assert_eq!(back.started_at, t0);
        assert_eq!(back.finished_at, t1);
        assert_eq!(back.manifest_id.as_deref(), Some("inventory-hw"));
    }

    #[test]
    fn exec_result_without_manifest_id_decodes() {
        // Older agents (pre-0.13) sent ExecResult with no manifest_id field.
        let json = r#"{
            "request_id":"r","pc_id":"x","exit_code":0,
            "stdout":"","stderr":"",
            "started_at":"2026-05-16T00:00:00Z",
            "finished_at":"2026-05-16T00:00:00Z"
        }"#;
        let r: ExecResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.manifest_id, None);
    }
}
