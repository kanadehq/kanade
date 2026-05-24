//! `support.upload_diagnostics` types — one-click "サポートに問い
//! 合わせる" diagnostics bundle.
//!
//! Per SPEC §2.1: the agent collects `{pc_id, recent_inventory,
//! last_N_events, agent_log_tail}`, zips them, uploads to the
//! JetStream Object Store, and the backend opens a helpdesk
//! ticket. The Client App shows the resulting ticket URL so the
//! user can paste it into the chat / email follow-up.

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
}
