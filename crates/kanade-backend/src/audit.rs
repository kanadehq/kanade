//! Audit-event publisher. Backend handlers call [`record`] after every
//! state-changing operation. Events flow on `audit.{actor}.{action}` and
//! land in the AUDIT stream (permanent retention per spec §2.3.1); the
//! audit projector then mirrors them into the SQLite `audit_log` table.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::Serialize;
use std::convert::Infallible;
use tracing::warn;

use crate::auth::Claims;

/// Header sent by the CLI (`X-Kanade-Source: cli`) and the SPA's
/// `apiFetch` (`X-Kanade-Source: spa`). Anything else (curl, custom
/// scripts, …) leaves [`Caller::source`] as `None`, which surfaces as
/// the absence of a `source` key in the audit payload.
pub const SOURCE_HEADER: &str = "x-kanade-source";

/// Per-request identity carried alongside every operator-initiated
/// audit event. Both fields are optional — auth-disable mode skips the
/// JWT decode (so `sub` is absent) and non-SPA / non-CLI clients have
/// no source header to read.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    pub sub: Option<String>,
    pub source: Option<String>,
}

impl<S> FromRequestParts<S> for Caller
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let sub = parts.extensions.get::<Claims>().map(|c| c.sub.clone());
        let source = parts
            .headers
            .get(SOURCE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        Ok(Caller { sub, source })
    }
}

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
///
/// When `caller` is `Some`, the caller's JWT subject and request
/// `X-Kanade-Source` header are merged into `payload` under the `sub`
/// and `source` keys. Existing keys in `payload` win — handlers can
/// still override either field if they need to.
pub async fn record(
    client: &async_nats::Client,
    actor: &str,
    action: &str,
    target: Option<&str>,
    caller: Option<&Caller>,
    mut payload: serde_json::Value,
) {
    if let Some(c) = caller
        && let Some(obj) = payload.as_object_mut()
    {
        if let Some(sub) = &c.sub {
            obj.entry("sub".to_string())
                .or_insert_with(|| sub.clone().into());
        }
        if let Some(src) = &c.source {
            obj.entry("source".to_string())
                .or_insert_with(|| src.clone().into());
        }
    }

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
