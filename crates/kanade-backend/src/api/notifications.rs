//! Phase E (KLP notifications) HTTP surface.
//!
//! - `POST /api/notifications` (operator+) — publish an end-user
//!   notification. Validates the audience, mints the id (when the
//!   operator didn't supply one) + `issued_at`, fans the body out to
//!   the `notifications.{all|group.X|pc.Y}` subjects (retained by the
//!   `NOTIFICATIONS` stream), and audits the send.
//! - `GET /api/notifications/{id}/ack_status` (viewer+) — list every
//!   `(pc_id, user_sid, acked_at)` recorded for the notification by the
//!   notification-acks projector, for the SPA's confirmation view.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use kanade_shared::ipc::notifications::{
    Notification, NotificationAckEntry, NotificationAckStatus, PublishNotificationRequest,
    PublishNotificationResponse,
};
use kanade_shared::subject;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::audit;
use crate::audit::Caller;

/// `POST /api/notifications` — publish an end-user notification.
pub async fn publish(
    State(s): State<AppState>,
    caller: Caller,
    Json(req): Json<PublishNotificationRequest>,
) -> Result<Json<PublishNotificationResponse>, (StatusCode, String)> {
    if !req.target.is_specified() {
        return Err((
            StatusCode::BAD_REQUEST,
            "target must set at least one of `all`, `groups`, or `pcs`".to_string(),
        ));
    }
    if req.title.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "title must not be empty".to_string(),
        ));
    }
    // Reject an already-past expiry — the Client App would hide the
    // notification the instant it arrived (dead on arrival), almost
    // always an operator typo rather than intent.
    if let Some(expires_at) = req.expires_at
        && expires_at <= chrono::Utc::now()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "expires_at must be in the future".to_string(),
        ));
    }

    // Operator-supplied id (the manifest's `id:`) wins; otherwise mint
    // one. v4 to match the rest of the backend's id minting (the uuid
    // dep ships without the v7 feature).
    let id = req
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let notification = Notification {
        id: id.clone(),
        priority: req.priority,
        require_ack: req.require_ack,
        title: req.title,
        body: req.body,
        issued_at: chrono::Utc::now(),
        issued_by: req.issued_by,
        expires_at: req.expires_at,
        // Fresh publish — never acked yet from anyone's perspective.
        acked_at: None,
    };

    // Resolve the audience into fan-out subjects, mirroring the exec
    // path's target → `commands.*` resolution.
    let mut subjects = Vec::new();
    if req.target.all {
        subjects.push(subject::NOTIFICATIONS_ALL.to_string());
    }
    for g in &req.target.groups {
        subjects.push(subject::notifications_group(g));
    }
    for pc in &req.target.pcs {
        subjects.push(subject::notifications_pc(pc));
    }

    let payload = serde_json::to_vec(&notification)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;

    // Acknowledged JetStream publish (not core `nats.publish`): each
    // call awaits a broker ack confirming the message landed in the
    // NOTIFICATIONS stream, so a backpressured / full broker surfaces
    // an error instead of silently dropping the notification. Fan-out
    // is best-effort — one failed subject doesn't abort delivery to
    // the rest (partial delivery beats none for a notification), and
    // the response echoes back only the subjects that actually
    // landed. The ack supersedes a manual `flush()`.
    let mut delivered = Vec::new();
    let mut failures = Vec::new();
    for subj in &subjects {
        let outcome = match s
            .jetstream
            .publish(subj.clone(), payload.clone().into())
            .await
        {
            Ok(ack) => ack.await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match outcome {
            Ok(_) => delivered.push(subj.clone()),
            Err(e) => {
                warn!(error = %e, subject = %subj, "notification publish failed");
                failures.push(subj.clone());
            }
        }
    }
    if delivered.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("all notification publishes failed for subjects: {failures:?}"),
        ));
    }

    info!(
        notification_id = %id,
        priority = ?notification.priority,
        require_ack = notification.require_ack,
        delivered = ?delivered,
        failed = ?failures,
        "notification published",
    );

    audit::record(
        &s.nats,
        "operator",
        "notification",
        Some(&id),
        Some(&caller),
        serde_json::json!({
            "notification_id": id,
            "priority": notification.priority,
            "require_ack": notification.require_ack,
            "subjects": delivered,
            "failed_subjects": failures,
        }),
    )
    .await;

    Ok(Json(PublishNotificationResponse {
        id,
        subjects: delivered,
    }))
}

/// `GET /api/notifications/{id}/ack_status` — per-recipient
/// confirmation list for one notification.
///
/// An empty `acks` array is intentionally **not** a 404: the
/// `notification_acks` table is an ack-only ledger, so "no rows" means
/// either nobody has confirmed yet OR the id was never sent — the two
/// are indistinguishable here by design (there is no separate
/// sent-ledger to cross-check against, and the audit projector that
/// records sends may lag). The SPA treats `acks: []` as "0 confirmed
/// so far" and pairs it with the operator's own send confirmation
/// (the `POST /api/notifications` response) to tell the cases apart.
pub async fn ack_status(
    State(pool): State<SqlitePool>,
    Path(id): Path<String>,
) -> Result<Json<NotificationAckStatus>, (StatusCode, String)> {
    let rows: Vec<(String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT pc_id, user_sid, acked_at
           FROM notification_acks
          WHERE notification_id = ?
          ORDER BY acked_at ASC",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query notification_acks: {e}"),
        )
    })?;

    let acks = rows
        .into_iter()
        .map(|(pc_id, user_sid, acked_at)| NotificationAckEntry {
            pc_id,
            user_sid,
            acked_at,
        })
        .collect();

    Ok(Json(NotificationAckStatus { id, acks }))
}
