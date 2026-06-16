use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Prefix injected into the UUIDv5 name string for deriving legacy
/// `result_id`s. Fixed marker so two backends (or one backend across
/// restarts) projecting the same legacy payload arrive at the same
/// id. Tied to the standard `Uuid::NAMESPACE_OID` namespace below.
/// Bumping this prefix would break dedupe of legacy redeliveries
/// crossing the upgrade — don't.
const LEGACY_RESULT_ID_PREFIX: &str = "kanade-issue-19/legacy-result-id:";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecResult {
    /// v0.29 / Issue #19: agent-minted UUID, unique per (Command, PC)
    /// run. Replaces `request_id` as the projector's primary key so
    /// broadcast Commands (commands.all / commands.group.X) — where N
    /// PCs share one `request_id` — finally persist all N results
    /// instead of silently dropping all but the first. Pre-v0.29
    /// agents omit this field; it deserialises as the empty string,
    /// and [`Self::stable_result_id`] derives a deterministic UUIDv5
    /// from `(request_id, pc_id)` so legacy payloads (a) get distinct
    /// ids across broadcast PCs (PC #2's row stops being dropped) and
    /// (b) get the SAME id on JetStream redelivery (the new `ON
    /// CONFLICT(result_id) DO NOTHING` path correctly dedupes, so
    /// `executions.success_count` doesn't double-count across retries).
    #[serde(default)]
    pub result_id: String,
    /// The NATS reply token. Still surfaced for joining back to the
    /// `kanade run` request/reply path. No longer unique across rows
    /// (broadcast Commands share it).
    pub request_id: String,
    /// v0.29 / Issue #19: back-link to `executions.exec_id`. Copied
    /// from `Command.exec_id` by the agent. `None` for ad-hoc
    /// `kanade run` (no deployment) and for results emitted by
    /// pre-v0.29 agents (decoded via `serde(default)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_id: Option<String>,
    pub pc_id: String,
    pub exit_code: i32,
    /// stdout. Empty string when [`Self::stdout_object`] is set — the
    /// agent overflowed the bytes into [`crate::kv::OBJECT_RESULT_OUTPUT`]
    /// because the inline payload would have exceeded NATS's default
    /// `max_payload` (#227). The backend projector derefs the pointer
    /// before inserting; SQLite still stores the full text inline so
    /// the SPA Activity page reads unchanged.
    pub stdout: String,
    pub stderr: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    /// Object Store key under [`crate::kv::OBJECT_RESULT_OUTPUT`] when
    /// `stdout` overflowed the agent's inline threshold (#227). Set to
    /// `Some("<request_id>/stdout")` by the agent's outbox drain; the
    /// backend projector fetches the bytes from that key and uses them
    /// in place of the (empty) `stdout` field. `None` for the common
    /// small-stdout case + every pre-#227 payload (`serde(default)`
    /// keeps older results decodable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_object: Option<String>,
    /// Sibling of `stdout_object` for the stderr stream. Same key
    /// shape (`<request_id>/stderr`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_object: Option<String>,
    /// v0.13: the manifest id that produced this result. Sourced
    /// from `Command.id` (which is the YAML `manifest.id`, e.g.
    /// `"inventory-hw"`). Distinct from the per-deploy UUID stored
    /// in `Command.exec_id`. The results projector uses this to
    /// look up the manifest's `inventory:` hint and upsert
    /// `inventory_facts` rows for inventory-tagged jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<String>,
    /// #219: Object Store key under [`crate::kv::OBJECT_COLLECTIONS`] for
    /// the bundle this run collected, when the job carried a `collect:`
    /// hint and the run succeeded. Set by the agent to
    /// `Some("<pc_id>/<job_id>/<rfc3339>.zip")` after it zips the
    /// script's listed files and uploads the archive. `None` for every
    /// non-collect job + every pre-#219 payload (`serde(default)` keeps
    /// older results decodable). The SPA Collect page lists / downloads
    /// these straight from the bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect_object: Option<String>,
}

impl ExecResult {
    /// Return the `result_id` if the agent supplied one (v0.29+
    /// payloads always do), otherwise derive a stable UUIDv5 from
    /// `(request_id, pc_id)`. The projector calls this before INSERT
    /// so legacy payloads still get a non-empty PK, AND so that
    /// JetStream redeliveries of the same legacy payload hash to the
    /// same id and dedupe via `ON CONFLICT`. Per-PC fan-out stays
    /// distinct (different `pc_id` → different hash).
    pub fn stable_result_id(&self) -> String {
        if !self.result_id.is_empty() {
            return self.result_id.clone();
        }
        let name = format!(
            "{LEGACY_RESULT_ID_PREFIX}{}:{}",
            self.request_id, self.pc_id
        );
        Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes()).to_string()
    }
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
            result_id: "result-uuid-1".into(),
            request_id: "req-1".into(),
            exec_id: Some("exec-uuid-1".into()),
            pc_id: "pc-01".into(),
            exit_code: 0,
            stdout: "hello\n".into(),
            stderr: String::new(),
            started_at: t0,
            finished_at: t1,
            stdout_object: None,
            stderr_object: None,
            manifest_id: Some("inventory-hw".into()),
            collect_object: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: ExecResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.result_id, r.result_id);
        assert_eq!(back.request_id, r.request_id);
        assert_eq!(back.exec_id.as_deref(), Some("exec-uuid-1"));
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

    #[test]
    fn exec_result_without_result_id_decodes_empty() {
        // v0.29 / Issue #19: pre-v0.29 agents don't send `result_id`.
        // `#[serde(default)]` decodes it as the empty string so the
        // projector can detect "legacy payload" and call
        // `stable_result_id()` to derive a deterministic PK.
        let json = r#"{
            "request_id":"r","pc_id":"x","exit_code":0,
            "stdout":"","stderr":"",
            "started_at":"2026-05-16T00:00:00Z",
            "finished_at":"2026-05-16T00:00:00Z"
        }"#;
        let r: ExecResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.result_id, "");
        assert!(r.exec_id.is_none());
    }

    #[test]
    fn stable_result_id_is_deterministic_for_legacy_payload() {
        // Gemini #65 medium fix: legacy redeliveries (same request_id +
        // pc_id) must hash to the SAME result_id so the projector's
        // ON CONFLICT(result_id) DO NOTHING dedupes — otherwise
        // `executions.success_count` double-counts on JetStream ack
        // timeouts.
        let json = r#"{
            "request_id":"r","pc_id":"x","exit_code":0,
            "stdout":"","stderr":"",
            "started_at":"2026-05-16T00:00:00Z",
            "finished_at":"2026-05-16T00:00:00Z"
        }"#;
        let a: ExecResult = serde_json::from_str(json).unwrap();
        let b: ExecResult = serde_json::from_str(json).unwrap();
        assert_eq!(
            a.stable_result_id(),
            b.stable_result_id(),
            "same legacy payload must hash to the same result_id",
        );
    }

    #[test]
    fn stable_result_id_differs_across_pcs_for_broadcast() {
        // The other half: a broadcast Command published to two PCs
        // produces two legacy ExecResults sharing one request_id but
        // with different pc_ids. Each must get its OWN result_id so
        // both rows persist (the whole point of Issue #19).
        let json_a = r#"{
            "request_id":"shared","pc_id":"pc-1","exit_code":0,
            "stdout":"","stderr":"",
            "started_at":"2026-05-16T00:00:00Z",
            "finished_at":"2026-05-16T00:00:00Z"
        }"#;
        let json_b = r#"{
            "request_id":"shared","pc_id":"pc-2","exit_code":0,
            "stdout":"","stderr":"",
            "started_at":"2026-05-16T00:00:00Z",
            "finished_at":"2026-05-16T00:00:00Z"
        }"#;
        let a: ExecResult = serde_json::from_str(json_a).unwrap();
        let b: ExecResult = serde_json::from_str(json_b).unwrap();
        assert_ne!(
            a.stable_result_id(),
            b.stable_result_id(),
            "different pc_id must produce a different result_id",
        );
    }

    #[test]
    fn stable_result_id_passes_through_explicit_value() {
        // v0.29 agents always supply result_id; the helper must
        // return that as-is (no surprise re-hashing).
        let r = ExecResult {
            result_id: "agent-minted-uuid".into(),
            request_id: "r".into(),
            exec_id: None,
            pc_id: "x".into(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap(),
            finished_at: chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap(),
            stdout_object: None,
            stderr_object: None,
            manifest_id: None,
            collect_object: None,
        };
        assert_eq!(r.stable_result_id(), "agent-minted-uuid");
    }

    #[test]
    fn exec_result_collect_object_round_trips_and_omits_when_absent() {
        // #219: collect_object is off the wire when None
        // (skip_serializing_if) so pre-#219 readers stay compatible...
        let t0 = chrono::Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap();
        let mut r = ExecResult {
            result_id: "r1".into(),
            request_id: "req".into(),
            exec_id: None,
            pc_id: "PC1".into(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            started_at: t0,
            finished_at: t0,
            stdout_object: None,
            stderr_object: None,
            manifest_id: Some("collect-diagnostics".into()),
            collect_object: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("collect_object"),
            "collect_object must be absent when None: {json}"
        );
        // ...and a set key survives the round-trip.
        r.collect_object = Some("PC1/collect-diagnostics/20260615T000000Z.zip".into());
        let back: ExecResult = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(
            back.collect_object.as_deref(),
            Some("PC1/collect-diagnostics/20260615T000000Z.zip"),
        );
    }
}
