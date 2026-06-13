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

use std::collections::HashMap;
use std::time::Duration;

use async_nats::jetstream::consumer::pull::Config as PullConfig;
use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use kanade_shared::ipc::notifications::{
    Notification, NotificationAckEntry, NotificationAckStatus, PublishNotificationRequest,
    PublishNotificationResponse,
};
use kanade_shared::kv::STREAM_NOTIFICATIONS;
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

/// Safety ceiling on how many stream messages `list_sent` replays in one
/// call (mirrors the agent's `notifications.list` cap). Notifications are
/// operator-broadcast, so a 90-day history is realistically dozens; this
/// only guards a runaway. Overflow keeps the freshest (rolling window).
const SENT_MAX_REPLAY: usize = 5000;
/// Per-fetch batch size when draining the stream.
const SENT_REPLAY_BATCH: usize = 500;
/// Cap on rows returned to the SPA after dedup (newest-first).
const SENT_MAX_ITEMS: usize = 200;

/// `GET /api/notifications` (viewer+) — the operator's sent-notification
/// history.
///
/// The backend has no sent-ledger table (the `notification_acks` table
/// is ack-only), so the source of truth for "what was sent" is the
/// `NOTIFICATIONS` JetStream stream itself. Replay it across the whole
/// fan-out space (`notifications.>`) via a throwaway ephemeral read-only
/// consumer — the same pattern the agent's `notifications.list` uses —
/// then dedup the per-subject copies (one publish fans the same id out to
/// `all` + each `group.X` + each `pc.Y`) back to one row per id and
/// return them newest-first. Powers the SPA's "what did I send" list,
/// each row deep-linking into its `ack_status` view.
///
/// Unlike the agent's per-user list this keeps **expired** notifications
/// (an operator reviewing history wants to see them) and does no ack
/// annotation — per-recipient confirmation lives behind `ack_status`.
pub async fn list_sent(
    State(s): State<AppState>,
) -> Result<Json<Vec<Notification>>, (StatusCode, String)> {
    let stream = s
        .jetstream
        .get_stream(STREAM_NOTIFICATIONS)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open {STREAM_NOTIFICATIONS} stream: {e}"),
            )
        })?;

    // Fast path for the empty stream (no notification ever sent): skip
    // the ephemeral-consumer create (a control-plane write) + the drain
    // entirely. `get_stream` already populated the cached info, so this
    // is free. A message landing between here and a real fetch would be
    // missed, but "0 → return empty" self-corrects on the next call.
    if stream.cached_info().state.messages == 0 {
        return Ok(Json(Vec::new()));
    }

    let consumer = stream
        .create_consumer(PullConfig {
            deliver_policy: DeliverPolicy::All,
            ack_policy: AckPolicy::None,
            // The NOTIFICATIONS stream only ever holds notifications.>
            // subjects; the explicit wildcard documents "every sent
            // notification, fleet-wide" (no audience scoping — operator
            // view).
            filter_subjects: vec!["notifications.>".to_string()],
            inactive_threshold: Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create ephemeral consumer: {e}"),
            )
        })?;

    // DeliverPolicy::All is oldest→newest; keep a rolling window of the
    // newest SENT_MAX_REPLAY so an over-cap stream still surfaces the
    // freshest sends (what an operator history cares about).
    // Size up front at the ceiling the rolling window allows (cap + the
    // one transient overflow entry) so a full stream doesn't realloc.
    let mut buf: std::collections::VecDeque<Notification> =
        std::collections::VecDeque::with_capacity(SENT_MAX_REPLAY + 1);
    let mut dropped = 0usize;
    loop {
        let mut batch = consumer
            .fetch()
            .max_messages(SENT_REPLAY_BATCH)
            // Short expiry: retained messages deliver near-instantly, so
            // the loop pays this window once on the drained tail.
            .expires(Duration::from_millis(200))
            .messages()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("fetch: {e}")))?;
        let mut got = 0usize;
        let mut exhausted = false;
        while let Some(m) = batch.next().await {
            let m = m.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("message: {e}")))?;
            got += 1;
            // The server reports how many messages remain pending for this
            // consumer; once it hits 0 we've drained the stream and can
            // stop without paying the next fetch's expiry window.
            if m.info().is_ok_and(|i| i.pending == 0) {
                exhausted = true;
            }
            match serde_json::from_slice::<Notification>(&m.payload) {
                Ok(n) => {
                    buf.push_back(n);
                    if buf.len() > SENT_MAX_REPLAY {
                        buf.pop_front();
                        dropped += 1;
                    }
                }
                Err(e) => warn!(
                    error = %e,
                    subject = %m.subject,
                    "list_sent: skipping unparseable notification",
                ),
            }
        }
        // Stop when the server says nothing's pending, or a short batch
        // already signalled the drained tail.
        if exhausted || got < SENT_REPLAY_BATCH {
            break;
        }
    }
    if dropped > 0 {
        warn!(
            dropped,
            cap = SENT_MAX_REPLAY,
            "list_sent: NOTIFICATIONS exceeded replay cap; oldest beyond the cap omitted",
        );
    }

    Ok(Json(dedup_newest_first(Vec::from(buf), SENT_MAX_ITEMS)))
}

/// Pure core of [`list_sent`]: collapse the per-subject fan-out copies to
/// one entry per id (keeping the newest `issued_at` if a malformed
/// publish ever repeated an id), sort newest-first, and cap at
/// `max_items`. Split out so it's unit-testable without a broker.
fn dedup_newest_first(raw: Vec<Notification>, max_items: usize) -> Vec<Notification> {
    let mut idx_of: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<Notification> = Vec::new();
    for n in raw {
        match idx_of.get(&n.id) {
            Some(&i) if n.issued_at <= deduped[i].issued_at => {}
            Some(&i) => deduped[i] = n,
            None => {
                idx_of.insert(n.id.clone(), deduped.len());
                deduped.push(n);
            }
        }
    }
    // Newest first; id breaks ties so equal-instant entries are stable.
    deduped.sort_by(|a, b| b.issued_at.cmp(&a.issued_at).then_with(|| a.id.cmp(&b.id)));
    deduped.truncate(max_items);
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::ipc::notifications::NotificationPriority;

    fn notif(id: &str, issued: chrono::DateTime<chrono::Utc>) -> Notification {
        Notification {
            id: id.into(),
            priority: NotificationPriority::Info,
            require_ack: false,
            title: "t".into(),
            body: "b".into(),
            issued_at: issued,
            issued_by: None,
            expires_at: None,
            acked_at: None,
        }
    }

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 6, 1, 12, 0, 0).unwrap()
            + chrono::Duration::seconds(secs)
    }

    #[test]
    fn dedups_fanout_copies_to_one_row_per_id() {
        // One publish to all + two groups → three identical-id copies.
        let raw = vec![notif("n1", at(0)), notif("n1", at(0)), notif("n1", at(0))];
        let out = dedup_newest_first(raw, 200);
        assert_eq!(out.len(), 1, "fan-out copies collapse to one");
        assert_eq!(out[0].id, "n1");
    }

    #[test]
    fn sorts_newest_first() {
        let raw = vec![
            notif("old", at(0)),
            notif("new", at(120)),
            notif("mid", at(60)),
        ];
        let out = dedup_newest_first(raw, 200);
        let ids: Vec<&str> = out.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old"]);
    }

    #[test]
    fn dedup_keeps_newest_issued_for_repeated_id() {
        let raw = vec![notif("dup", at(0)), notif("dup", at(60))];
        let out = dedup_newest_first(raw, 200);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].issued_at, at(60), "newest issued_at wins");
    }

    #[test]
    fn caps_at_max_items_after_sort() {
        // 3 distinct, cap 2 → the two newest survive.
        let raw = vec![notif("a", at(0)), notif("b", at(60)), notif("c", at(120))];
        let out = dedup_newest_first(raw, 2);
        let ids: Vec<&str> = out.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b"], "newest two kept");
    }
}
