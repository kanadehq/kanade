//! `support.*` types — the helpdesk-facing corner of KLP.
//!
//! Two features share the namespace:
//!
//! - `support.upload_diagnostics` — one-click "サポートに問い合わせる"
//!   diagnostics bundle. Per SPEC §2.1: the agent collects
//!   `{pc_id, recent_inventory, last_N_events, agent_log_tail}`, zips
//!   them, uploads to the JetStream Object Store, and the backend opens a
//!   helpdesk ticket. The Client App shows the resulting ticket URL so
//!   the user can paste it into the chat / email follow-up.
//! - `support.unlock` / `support.lock` / `support.status` — the
//!   operator-code display gate in front of `client.unlock`-scoped jobs: the
//!   IT desk types a secret code on the user's PC to reveal actions that have
//!   no business sitting in that user's everyday catalog. Listing-only — see
//!   `kanade_shared::manifest::ClientHint::unlock`.

use serde::{Deserialize, Serialize};

/// `support.upload_diagnostics` params — optional user-supplied
/// context so the helpdesk has triage info at ticket-open time
/// without a second round-trip.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct SupportUploadDiagnosticsParams {
    /// One-line summary the user typed into the support form
    /// (e.g. "Teams won't open since the update"). May be empty.
    #[serde(default)]
    pub summary: String,
    /// Optional longer description / repro steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// User's contact preference (email / Teams handle / phone).
    /// Free-form because organisations differ; the SPA presents a
    /// drop-down but stores the chosen value as a string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
}

/// `support.upload_diagnostics` response.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct SupportUploadDiagnosticsResult {
    /// JetStream Object Store key for the uploaded zip — used by
    /// the helpdesk's tooling to fetch the bundle without
    /// re-asking the user to attach it.
    pub object_key: String,
    /// Ticket id from whichever helpdesk system the backend
    /// integrated with (Jira, ServiceNow, …). `None` when the
    /// upload succeeded but ticket creation deferred (the backend
    /// retries asynchronously).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    /// User-friendly URL to view the ticket (or, when `ticket_id`
    /// is None, a generic "your diagnostics have been uploaded"
    /// landing page). The Client App shows this as the post-submit
    /// confirmation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_url: Option<String>,
    /// Size of the uploaded zip in bytes. Surfaced so the SPA can
    /// show "Uploaded 4.2 MB" — reassuring proof that the bundle
    /// went through.
    pub size_bytes: u64,
}

// ---------- support.unlock / support.lock / support.status ----------

/// One live unlock grant — every `client.unlock: <scope>` job is listed for
/// the caller until `expires_at`.
///
/// Grants are held per **OS user** (the connection's SID), not per
/// connection: the Client App reconnects on its own (a pipe hiccup, a
/// restart), and silently re-locking mid-support-call would be a
/// baffling failure mode. `support.status` lets a reconnecting client
/// recover the banner without re-asking for the code.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct UnlockGrant {
    /// The `client.unlock` scope this grant opens (`support`, `admin`, …).
    pub scope: String,
    /// Operator-supplied human label for the code that opened it (from
    /// `ServerSettings::support_codes[].label`), so the Client App banner
    /// can read「サポートモード（ヘルプデスク）」rather than a bare slug.
    /// `None` when the operator left it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// When the grant lapses on its own. The Client App counts down to it
    /// and re-locks its UI at zero; the agent enforces it independently at
    /// every `jobs.list` / `jobs.execute`, so a client that ignores the
    /// deadline gains nothing.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// `support.unlock` params — the code the IT desk typed into the Client
/// App. Compared against the argon2id hashes in
/// `ServerSettings::support_codes`; the plaintext never leaves the agent
/// process (it is not logged, and audit events record only the outcome).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct SupportUnlockParams {
    pub code: String,
}

/// `support.unlock` response — every grant the caller holds after the
/// redeem, not just the newly-added one, so the client can render the
/// whole banner from one reply.
///
/// A wrong code does NOT come back here: it's an
/// [`ErrorKind::Unauthorized`](crate::ipc::error::ErrorKind::Unauthorized)
/// error, and repeated failures escalate to
/// [`ErrorKind::RateLimit`](crate::ipc::error::ErrorKind::RateLimit).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct SupportUnlockResult {
    pub grants: Vec<UnlockGrant>,
}

/// `support.lock` params — no selectors: locking is all-or-nothing. The
/// button means "I'm done, close it all", and a partial lock would leave
/// a scope open that the banner no longer advertises.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct SupportLockParams {}

/// `support.lock` response — how many grants were dropped (0 when the
/// caller held none; locking is idempotent, never an error).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct SupportLockResult {
    pub released: usize,
}

/// `support.status` params — no selectors; the answer is always "what
/// does the calling OS user hold right now".
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct SupportStatusParams {}

/// `support.status` response — the caller's live grants (empty ⇒ locked).
/// Expired grants are swept before answering, so every entry here is
/// currently in force.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct SupportStatusResult {
    pub grants: Vec<UnlockGrant>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_default_is_empty_summary() {
        let p = SupportUploadDiagnosticsParams::default();
        assert_eq!(p.summary, "");
        assert!(p.detail.is_none());
        assert!(p.contact.is_none());
    }

    #[test]
    fn params_minimal_wire_decodes() {
        let p: SupportUploadDiagnosticsParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.summary, "");
    }

    #[test]
    fn result_with_ticket_round_trips() {
        let r = SupportUploadDiagnosticsResult {
            object_key: "support/2026-05-24/abc123.zip".into(),
            ticket_id: Some("HELP-42".into()),
            ticket_url: Some("https://helpdesk.example.com/tickets/HELP-42".into()),
            size_bytes: 4_200_000,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: SupportUploadDiagnosticsResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.object_key, r.object_key);
        assert_eq!(back.ticket_id, r.ticket_id);
        assert_eq!(back.ticket_url, r.ticket_url);
        assert_eq!(back.size_bytes, r.size_bytes);
    }

    #[test]
    fn result_without_ticket_omits_field_on_wire() {
        // Deferred-ticket path: object uploaded, ticket id pending.
        // SPA UI key: ticket_id absence ⇒ "uploaded, ticket
        // pending". Wire MUST be field-absent, not null.
        let r = SupportUploadDiagnosticsResult {
            object_key: "x".into(),
            ticket_id: None,
            ticket_url: None,
            size_bytes: 0,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("ticket_id").is_none(), "wire: {v:?}");
        assert!(v.get("ticket_url").is_none(), "wire: {v:?}");
    }

    #[test]
    fn unlock_grant_omits_absent_label() {
        // The banner falls back to the scope slug when the operator left
        // `label` unset — the wire must be field-absent, not null (strict
        // JS clients reject `null` where they expect `string | undefined`).
        let g = UnlockGrant {
            scope: "support".into(),
            label: None,
            expires_at: chrono::Utc::now(),
        };
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["scope"], "support");
        assert!(v.get("label").is_none(), "wire: {v:?}");
    }

    #[test]
    fn unlock_result_round_trips() {
        let wire = r#"{"grants":[
            {"scope":"support","label":"ヘルプデスク","expires_at":"2026-07-28T09:00:00Z"}
        ]}"#;
        let r: SupportUnlockResult = serde_json::from_str(wire).unwrap();
        assert_eq!(r.grants.len(), 1);
        assert_eq!(r.grants[0].scope, "support");
        assert_eq!(r.grants[0].label.as_deref(), Some("ヘルプデスク"));
    }

    #[test]
    fn status_result_empty_means_locked() {
        // No grants ⇒ an empty array, never a missing key: the client
        // treats `grants.length === 0` as "locked" and must not have to
        // distinguish that from a malformed reply.
        let r = SupportStatusResult { grants: vec![] };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["grants"], serde_json::json!([]));
    }
}
