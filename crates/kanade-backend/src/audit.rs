//! Audit-event publisher. Backend handlers call [`record`] after every
//! state-changing operation. Events flow on `audit.{actor}.{action}` and
//! land in the AUDIT stream (permanent retention per spec §2.3.1); the
//! audit projector then mirrors them into the SQLite `audit_log` table.

use serde::Serialize;
use tracing::warn;

#[derive(Serialize)]
pub struct AuditEvent<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub target: Option<&'a str>,
    pub payload: serde_json::Value,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
}

/// Build the subject + body for an audit event and publish on NATS. The
/// helper is best-effort: a publish failure is logged at WARN but does
/// not propagate, because the caller has already done the real work.
pub async fn record(
    client: &async_nats::Client,
    actor: &str,
    action: &str,
    target: Option<&str>,
    payload: serde_json::Value,
) {
    let subject = match target {
        Some(t) => format!("audit.{actor}.{action}.{t}"),
        None => format!("audit.{actor}.{action}"),
    };
    let event = AuditEvent {
        actor,
        action,
        target,
        payload,
        occurred_at: chrono::Utc::now(),
    };
    let body = match serde_json::to_vec(&event) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, subject = %subject, "audit serialize failed");
            return;
        }
    };
    if let Err(e) = client.publish(subject.clone(), body.into()).await {
        warn!(error = %e, subject = %subject, "audit publish failed");
    }
}
