//! Best-effort audit-event publisher for the CLI's NATS-direct
//! mutations.
//!
//! The backend records an audit event for every state-changing HTTP
//! call (`kanade-backend::audit::record`), but the CLI's fleet
//! mutations — `agent publish/rollout`, `app publish`, `script
//! publish`, `job create`, `exec` — go STRAIGHT to NATS and never
//! touch the backend, so they used to leave no trace in the SPA's
//! audit log. This module closes that gap by publishing the same
//! wire shape onto the same `audit.{actor}.{action}[.{target}]`
//! subjects (captured by the AUDIT stream's `audit.>` filter and
//! mirrored into `audit_log` by the backend's audit projector).
//!
//! Conventions mirror the backend so the audit log reads uniformly:
//! `actor` is the taxonomy bucket (`operator` for fleet mutations),
//! the human identity rides in `payload.sub` ($KANADE_ACTOR, else
//! the OS username) and `payload.source` is `cli`.
//!
//! Best-effort by design: a publish failure logs a WARN but never
//! fails the command — the real work has already happened.

use serde::Serialize;
use tracing::warn;

#[derive(Serialize)]
struct AuditEvent<'a> {
    actor: &'a str,
    action: &'a str,
    target: Option<&'a str>,
    payload: serde_json::Value,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

/// Who is driving this CLI? `$KANADE_ACTOR` wins (ops terminals /
/// CI can pin a meaningful identity), else the OS username, else
/// a literal `unknown`.
fn operator_sub() -> String {
    std::env::var("KANADE_ACTOR")
        .or_else(|_| std::env::var("USERNAME"))
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Publish one audit event for a CLI-side fleet mutation. `actor`
/// is fixed to `operator` (the backend's taxonomy bucket for fleet
/// mutations); pass the same `action` literal the equivalent backend
/// handler uses (`agent_publish`, `agent_rollout`,
/// `app_package_publish`, `script_object_publish`, `job_upsert`,
/// `exec`, …) so SPA filters see one vocabulary.
pub async fn record(
    client: &async_nats::Client,
    action: &str,
    target: Option<&str>,
    mut payload: serde_json::Value,
) {
    const ACTOR: &str = "operator";
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("sub".to_string())
            .or_insert_with(|| operator_sub().into());
        obj.entry("source".to_string())
            .or_insert_with(|| "cli".into());
    }
    let subject = match target {
        Some(t) => format!("audit.{ACTOR}.{action}.{t}"),
        None => format!("audit.{ACTOR}.{action}"),
    };
    let event = AuditEvent {
        actor: ACTOR,
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
        return;
    }
    // publish() only buffers — the CLI process exits right after most
    // commands, racing the background flusher, and a lost race silently
    // drops the event (live test: 3 of 4 deploy-flow audits landed, the
    // one straight after a 42 MB upload didn't). Flush so the event is
    // on the wire before the command returns.
    if let Err(e) = client.flush().await {
        warn!(error = %e, subject = %subject, "audit flush failed");
    }
}
