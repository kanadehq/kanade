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

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_nats::jetstream::consumer::pull::Config as PullConfig;
use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use kanade_shared::ipc::notifications::{
    AudiencePc, EditNotificationRequest, Notification, NotificationAckEntry, NotificationAckStatus,
    NotificationAmend, NotificationAmendOp, NotificationDetail, NotificationTarget,
    PublishNotificationRequest, PublishNotificationResponse,
};
use kanade_shared::kv::STREAM_NOTIFICATIONS;
use kanade_shared::subject;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::api::agent_groups;
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
        toast: req.toast,
        issued_at: chrono::Utc::now(),
        issued_by: req.issued_by,
        expires_at: req.expires_at,
        // Fresh publish — never acked yet from anyone's perspective.
        acked_at: None,
        // Never edited / reset on a fresh publish.
        edited_at: None,
        acks_reset_at: None,
    };

    let (delivered, failures) = fan_out_notification(&s.jetstream, &notification, &req.target)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
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

/// Resolve a notification's audience [`Target`] into the
/// `notifications.{all|group.X|pc.Y}` fan-out subjects and
/// acknowledged-JetStream-publish the body to each, returning
/// `(delivered, failed)` subjects. Best-effort: one failed subject doesn't
/// abort the rest (partial delivery beats none). Shared by the operator
/// HTTP [`publish`] and the compliance-alert projector path so the two
/// fan out identically. `Err` only on a serialize failure (the publish
/// errors are folded into `failed`).
pub(crate) async fn fan_out_notification(
    js: &async_nats::jetstream::Context,
    notification: &Notification,
    target: &kanade_shared::manifest::Target,
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut subjects = Vec::new();
    if target.all {
        subjects.push(subject::NOTIFICATIONS_ALL.to_string());
    }
    for g in &target.groups {
        subjects.push(subject::notifications_group(g));
    }
    for pc in &target.pcs {
        subjects.push(subject::notifications_pc(pc));
    }

    // Serialize once into `Bytes`: `js.publish` takes `bytes::Bytes`, whose
    // clone is a cheap refcount bump (not a full buffer copy per subject).
    let payload: bytes::Bytes = serde_json::to_vec(notification)
        .map_err(|e| format!("serialize: {e}"))?
        .into();

    let mut delivered = Vec::new();
    let mut failures = Vec::new();
    for subj in &subjects {
        let outcome = match js.publish(subj.clone(), payload.clone()).await {
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
    Ok((delivered, failures))
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
    let acks = fetch_acks(&pool, &id).await?;
    Ok(Json(NotificationAckStatus { id, acks }))
}

/// Read every recorded confirmation for one notification, oldest-first.
/// Shared by [`ack_status`] and [`detail`] so the two stay in lock-step.
///
/// `account` is a human-readable label for who confirmed (⑤): the login
/// name the agent recorded with the ack when available, else — for acks
/// recorded before agents emitted it — the PC's last-logon display name
/// or login from the `agents` row (a best-effort fallback that's exact on
/// single-user PCs and a reasonable approximation otherwise). When neither
/// exists the field is `None` and the SPA shows the SID. The `LEFT JOIN`
/// keeps acks from PCs with no `agents` row (e.g. de-registered hosts).
async fn fetch_acks(
    pool: &SqlitePool,
    id: &str,
) -> Result<Vec<NotificationAckEntry>, (StatusCode, String)> {
    let rows: Vec<(
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT na.pc_id, na.user_sid, na.acked_at,
                    COALESCE(na.account, a.last_logon_display_name, a.last_logon_user)
               FROM notification_acks na
               LEFT JOIN agents a ON a.pc_id = na.pc_id
              WHERE na.notification_id = ?
              ORDER BY na.acked_at ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query notification_acks: {e}"),
        )
    })?;

    Ok(rows
        .into_iter()
        .map(
            |(pc_id, user_sid, acked_at, account)| NotificationAckEntry {
                pc_id,
                user_sid,
                acked_at,
                account,
            },
        )
        .collect())
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
    let raw = replay_all_sent(&s).await?;
    // The history list doesn't need the per-copy subjects or stream seqs
    // (those are detail-page / recall concerns), so drop them before dedup.
    let notifs = raw.into_iter().map(|(n, _subj, _seq)| n).collect();
    Ok(Json(dedup_newest_first(notifs, SENT_MAX_ITEMS)))
}

/// `GET /api/notifications/{id}` (viewer+) — one sent notification's full
/// content plus its confirmation list, for the deep-linkable detail page.
///
/// Same stream source as [`list_sent`] (the NOTIFICATIONS stream is the
/// only record of what was sent), filtered down to the requested id: the
/// history table only carries the truncated columns, so the detail page
/// re-fetches the full body here — which also makes the page work on a
/// cold deep link (Ctrl/⌘-click → new tab) where no client-side state
/// carried the notification over. A missing id is a real 404 (unlike
/// `ack_status`, which can't tell "never sent" from "sent, not yet
/// acked"): the stream IS the sent-ledger, so absence here is
/// authoritative.
pub async fn detail(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<NotificationDetail>, (StatusCode, String)> {
    let raw = replay_all_sent(&s).await?;
    // Walk the raw replay once, keeping the requested id's newest fan-out
    // copy AND collecting every subject it landed on. NOT via
    // `dedup_newest_first(.., SENT_MAX_ITEMS)`: that caps the result at the
    // 200 newest *distinct* notifications, so deep-linking one older than
    // the 200th-newest — but still inside the 5000-message replay window —
    // would 404 spuriously. The subjects are the only record of who the
    // notification was addressed to (the body carries no target), so we
    // capture them here to reconstruct the audience below.
    let mut notification: Option<Notification> = None;
    let mut subjects: Vec<String> = Vec::new();
    for (n, subj, _seq) in raw {
        if n.id != id {
            continue;
        }
        subjects.push(subj);
        match &notification {
            Some(prev) if n.issued_at <= prev.issued_at => {}
            _ => notification = Some(n),
        }
    }
    let notification = notification.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("notification {id} not found"),
        )
    })?;

    let acks = fetch_acks(&s.pool, &id).await?;
    let target = Some(parse_notification_target(&subjects));
    let audience = resolve_audience(&s, &subjects, &acks).await?;
    Ok(Json(NotificationDetail {
        notification,
        acks,
        audience,
        target,
    }))
}

/// `POST /api/notifications/{id}/recall` (operator+) — completely delete a
/// sent notification.
///
/// Unlike letting it lapse at `expires_at` (which only hides it on clients),
/// recall is a *full* removal an operator reaches for after a misfire: every
/// fan-out copy of the id is physically deleted from the `NOTIFICATIONS`
/// stream, so it vanishes from the history (`list_sent`), the detail page,
/// and any client's `notifications.list` re-sync. A live amend broadcast then
/// pulls it off the screens of clients currently showing it.
///
/// Flow: replay the stream → collect every `stream_sequence` carrying this id
/// (404 if none — the stream is the authoritative sent-ledger, so absence is
/// "never sent / already recalled") → `delete_message` each (best-effort: a
/// partial failure still removes what it can and is logged) → broadcast a
/// `Recall` amend to the fleet → audit.
///
/// ⚠ Already-delivered OS toasts (Action Center) can't be programmatically
/// dismissed — recall clears the in-app panel / unread / ack modal, but a
/// toast that already surfaced stays until the user clears it.
pub async fn recall(
    State(s): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let raw = replay_all_sent(&s).await?;
    let seqs: Vec<u64> = raw
        .iter()
        .filter(|(n, _, _)| n.id == id)
        .map(|(_, _, seq)| *seq)
        .collect();
    if seqs.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("notification {id} not found"),
        ));
    }

    let (deleted, failed) = delete_stream_msgs(&s, &seqs).await?;
    // Recall is all-or-nothing: only a fully-deleted notification may report
    // success + broadcast the live amend. On a partial delete (`failed > 0`)
    // the un-deleted copies would resurrect on the next `notifications.list`
    // re-sync, so clearing live panels would be a lie — and `deleted == 0`
    // (every delete failed, or every seq was the info-failed `0` sentinel)
    // means nothing was removed at all. Either way, return an error and don't
    // broadcast: the operator retries, and since the already-deleted copies
    // are gone a retry replays only the remainder, converging once the broker
    // recovers. (CodeRabbit #771.)
    if deleted == 0 || failed > 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "failed to fully recall notification {id}: deleted {deleted}, failed {failed} of {} copies",
                seqs.len()
            ),
        ));
    }

    // Pull it off the screens of clients showing it right now. Best-effort
    // (core NATS, fire-and-forget): offline clients reconcile on reconnect
    // via `notifications.list`, which no longer returns the deleted id.
    fan_out_amend(
        &s,
        &NotificationAmend {
            id: id.clone(),
            op: NotificationAmendOp::Recall,
        },
    )
    .await;

    info!(
        notification_id = %id,
        deleted,
        "notification recalled",
    );

    audit::record(
        &s.nats,
        "operator",
        "notification.recall",
        Some(&id),
        Some(&caller),
        serde_json::json!({
            "notification_id": id,
            // Always == seqs.len() here: a partial delete returned early above.
            "deleted_copies": deleted,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/notifications/{id}` (operator+) — edit a sent notification's
/// content in place: title, body, expiry, priority, require_ack, toast. The
/// **audience is immutable** (re-targeting = recall → re-send), and `id`,
/// `issued_at` (the "sent at"), and `issued_by` are preserved.
///
/// Flow (builds on recall's primitives): replay to find the notification + its
/// fan-out subjects + sequences (404 if none) → delete the old stream copies
/// (all-or-nothing, like recall, so a partial failure never leaves a duplicate
/// id with stale content) → re-publish the merged notification to the SAME
/// subjects with the same id + `issued_at`, stamping `edited_at`. Connected
/// clients receive the re-published copy through the normal `notifications.new`
/// path and replace the content in place (the client recognises a same-id,
/// same-`issued_at` arrival as an edit, not a fresh toast); offline clients
/// pick it up on the next `notifications.list`.
///
/// `reset_acks` additionally stamps `acks_reset_at` and clears the projected
/// ack ledger, forcing re-confirmation of a materially-changed body. Per-user
/// read marks are NOT scanned/deleted: the agent's `notifications.list` treats
/// a read mark older than `acks_reset_at` as unread, so end users re-confirm
/// without an O(fleet) KV sweep.
///
/// ⚠ Same OS-toast limitation as recall: an already-surfaced toast keeps its
/// original text (OS-managed); only the in-app panel reflects the new content.
pub async fn edit(
    State(s): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(req): Json<EditNotificationRequest>,
) -> Result<Json<PublishNotificationResponse>, (StatusCode, String)> {
    if req.title.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "title must not be empty".to_string(),
        ));
    }
    // Note: unlike `publish`, a past `expires_at` is intentionally allowed here
    // — it's a deliberate "retire it but keep history" edit, distinct from a
    // DOA typo on a fresh send.

    let raw = replay_all_sent(&s).await?;
    // Keep the newest copy of the id AND every subject/sequence it landed on
    // (mirrors `detail` + `recall`).
    let mut existing: Option<Notification> = None;
    let mut subjects: Vec<String> = Vec::new();
    let mut seqs: Vec<u64> = Vec::new();
    for (n, subj, seq) in raw {
        if n.id != id {
            continue;
        }
        subjects.push(subj);
        seqs.push(seq);
        match &existing {
            Some(prev) if n.issued_at <= prev.issued_at => {}
            _ => existing = Some(n),
        }
    }
    let existing = existing.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("notification {id} not found"),
        )
    })?;

    let now = chrono::Utc::now();
    // A new reset wins; otherwise carry any prior reset forward so a later
    // typo-fix edit (reset_acks=false) doesn't un-reset an earlier reset.
    let acks_reset_at = if req.reset_acks {
        Some(now)
    } else {
        existing.acks_reset_at
    };
    let merged = Notification {
        id: existing.id.clone(),
        priority: req.priority,
        require_ack: req.require_ack,
        title: req.title,
        body: req.body,
        toast: req.toast,
        // Preserve identity + "sent at" so the edit isn't a re-send.
        issued_at: existing.issued_at,
        issued_by: existing.issued_by.clone(),
        expires_at: req.expires_at,
        acked_at: None,
        edited_at: Some(now),
        acks_reset_at,
    };

    // Audience is immutable on edit — re-publish to the very subjects the
    // original landed on.
    let parsed = parse_notification_target(&subjects);
    let target = kanade_shared::manifest::Target {
        all: parsed.all,
        groups: parsed.groups,
        pcs: parsed.pcs,
    };

    // Delete the old copies first (all-or-nothing) so the re-publish never
    // produces a duplicate id with stale content. A partial delete aborts
    // before re-publishing — same reasoning as recall.
    let (deleted, failed) = delete_stream_msgs(&s, &seqs).await?;
    if deleted == 0 || failed > 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "failed to replace notification {id}: deleted {deleted}, failed {failed} of {} copies; not re-publishing to avoid a duplicate",
                seqs.len()
            ),
        ));
    }

    // Re-publish the merged notification to the same audience. Because the old
    // copies are already fully deleted, a PARTIAL re-publish is data loss — the
    // failed subjects now have no copy at all — so (unlike `publish`, which
    // deletes nothing) edit treats any `pub_failures` as a hard error, not just
    // the all-failed case. The operator recovers with recall → re-send.
    // (claude #774.)
    let (delivered, pub_failures) = fan_out_notification(&s.jetstream, &merged, &target)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if delivered.is_empty() || !pub_failures.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "deleted the old copies of {id} but re-publish failed for {pub_failures:?} (delivered {delivered:?}); the notification is now missing from the failed subjects — recall and re-send it"
            ),
        ));
    }

    // Reset confirmations if asked: clear the projected ack ledger so the
    // operator's confirmation view shows everyone unconfirmed. (End users are
    // handled by `acks_reset_at` in `notifications.list`; no KV sweep here.)
    // Non-fatal: the edit already shipped, and `acks_reset_at` still drives the
    // end-user re-confirm — but record the outcome in the audit (below) so a
    // failed clear (a stale operator "who confirmed" view) is visible in the
    // persisted record, not only a transient warn log. (claude #774.)
    let acks_cleared = if req.reset_acks {
        match sqlx::query("DELETE FROM notification_acks WHERE notification_id = ?")
            .bind(&id)
            .execute(&s.pool)
            .await
        {
            Ok(_) => true,
            Err(e) => {
                warn!(error = %e, notification_id = %id, "edit: clearing notification_acks failed");
                false
            }
        }
    } else {
        // No reset requested — nothing to clear (don't imply a failure).
        true
    };

    info!(
        notification_id = %id,
        delivered = ?delivered,
        reset_acks = req.reset_acks,
        acks_cleared,
        "notification edited",
    );

    audit::record(
        &s.nats,
        "operator",
        "notification.edit",
        Some(&id),
        Some(&caller),
        serde_json::json!({
            "notification_id": id,
            "subjects": delivered,
            "reset_acks": req.reset_acks,
            // false ⇒ reset was asked but the ledger clear failed; the operator
            // confirmation view may be stale (end users still re-confirm).
            "acks_cleared": acks_cleared,
        }),
    )
    .await;

    Ok(Json(PublishNotificationResponse {
        id,
        subjects: delivered,
    }))
}

/// Physically delete the given `NOTIFICATIONS` stream messages by sequence,
/// returning `(deleted, failed)` counts. Best-effort: a single failed delete
/// is logged and counted but doesn't abort the rest (a partial removal still
/// beats leaving the whole notification live). The caller collects every seq
/// first, then deletes, so a mid-iteration error can't leave seqs un-visited.
async fn delete_stream_msgs(
    s: &AppState,
    seqs: &[u64],
) -> Result<(u64, u64), (StatusCode, String)> {
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

    let mut deleted = 0u64;
    let mut failed = 0u64;
    for &seq in seqs {
        // 0 is the `replay_all_sent` sentinel for a copy whose `info()`
        // failed (no real JetStream message has sequence 0); skip it rather
        // than pay a roundtrip that always fails + logs a false warning.
        if seq == 0 {
            continue;
        }
        match stream.delete_message(seq).await {
            Ok(_) => deleted += 1,
            Err(e) => {
                warn!(error = %e, seq, "recall: delete_message failed");
                failed += 1;
            }
        }
    }
    Ok((deleted, failed))
}

/// Broadcast a post-send [`NotificationAmend`] to the whole fleet on the
/// ephemeral [`subject::NOTIFICATIONS_AMEND_SUBJECT`] core-NATS channel. Every
/// agent subscribes and forwards a `notifications.amended` push to its
/// clients; a client applies it only if it holds the referenced id, so this
/// single subject reaches everyone without per-audience routing. Fire-and-
/// forget (no JetStream ack) — the durable half of the operation already
/// happened (the stream delete); this is just the live screen update.
async fn fan_out_amend(s: &AppState, amend: &NotificationAmend) {
    let payload = match serde_json::to_vec(amend) {
        Ok(b) => bytes::Bytes::from(b),
        Err(e) => {
            warn!(error = %e, "fan_out_amend: serialize failed");
            return;
        }
    };
    if let Err(e) = s
        .nats
        .publish(subject::NOTIFICATIONS_AMEND_SUBJECT, payload)
        .await
    {
        warn!(error = %e, "fan_out_amend: publish failed");
    }
}

/// Reconstruct the per-PC confirmation roster (④) for a notification from
/// the fan-out subjects it was published to, joined against its recorded
/// acks.
///
/// The notification body carries no audience, so the only record of who it
/// was addressed to is the set of `notifications.{all|group.X|pc.Y}`
/// subjects its copies landed on (captured in [`detail`]). Expand those to
/// the expected PC set:
/// - `notifications.all` → every PC in the `agents` table (the registered
///   fleet);
/// - `notifications.group.X` → the PCs in group `X` (via the `agent_groups`
///   bucket);
/// - `notifications.pc.Y` → PC `Y` directly.
///
/// Then flag each expected PC confirmed/pending by joining the acks (PC
/// granularity: a PC is confirmed once *any* of its users acked), attach
/// the host's last-logon identity from `agents`, and sort pending-first so
/// "who hasn't confirmed" is at the top. Any PC that acked is always
/// included even if it's since fallen out of the resolved audience (a
/// group membership change after the send), so a real confirmation never
/// vanishes from the roster.
async fn resolve_audience(
    s: &AppState,
    subjects: &[String],
    acks: &[NotificationAckEntry],
) -> Result<Vec<AudiencePc>, (StatusCode, String)> {
    let all = subjects.iter().any(|s| s == subject::NOTIFICATIONS_ALL);
    let needs_groups = subjects
        .iter()
        .any(|s| s.starts_with(subject::NOTIFICATIONS_GROUP_PREFIX));

    // Only pay the agent_groups KV walk when a group was actually targeted.
    let membership = if needs_groups {
        agent_groups::membership_map(s).await
    } else {
        HashMap::new()
    };

    // Load last-logon identity per PC. For a broadcast (`all`) we need the
    // whole fleet; for a group/pc-scoped send we only need the candidate
    // PCs, so scope the query to them rather than scanning every agent row
    // (the common case on a large fleet — a targeted send shouldn't read
    // the whole table). `assemble_roster` re-derives the same expected set,
    // so the rows we skip here would only have been filtered out there.
    let agent_rows = if all {
        load_agents(s, None).await?
    } else {
        let mut candidates: HashSet<String> = HashSet::new();
        for subj in subjects {
            if let Some(pc) = subj.strip_prefix(subject::NOTIFICATIONS_PC_PREFIX) {
                candidates.insert(pc.to_string());
            }
        }
        for a in acks {
            candidates.insert(a.pc_id.clone());
        }
        for (pc_id, pc_groups) in &membership {
            if pc_groups
                .iter()
                .any(|g| subjects.contains(&subject::notifications_group(g)))
            {
                candidates.insert(pc_id.clone());
            }
        }
        if candidates.is_empty() {
            Vec::new()
        } else {
            load_agents(s, Some(candidates)).await?
        }
    };

    Ok(assemble_roster(subjects, &agent_rows, &membership, acks))
}

/// Load `(pc_id, last_logon_user, last_logon_display_name)` from `agents`.
/// `only = None` reads the whole fleet (the `all` broadcast case); `Some`
/// scopes to a candidate PC set via a parameter-bound `IN (…)` clause so a
/// targeted send doesn't scan every agent row.
async fn load_agents(
    s: &AppState,
    only: Option<HashSet<String>>,
) -> Result<Vec<(String, Option<String>, Option<String>)>, (StatusCode, String)> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT pc_id, last_logon_user, last_logon_display_name FROM agents",
    );
    if let Some(candidates) = only {
        qb.push(" WHERE pc_id IN (");
        let mut sep = qb.separated(", ");
        for pc in candidates {
            sep.push_bind(pc);
        }
        sep.push_unseparated(")");
    }
    qb.build_query_as().fetch_all(&s.pool).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query agents for audience: {e}"),
        )
    })
}

/// Pure core of [`resolve_audience`]: turn the captured fan-out
/// `subjects`, the fleet's `agent_rows` (`pc_id`, last-logon user/display),
/// the `pc_id -> [group]` `membership` map, and the recorded `acks` into
/// the per-PC roster. Split out so the expansion / ack-join / ordering is
/// unit-testable without a broker or DB.
/// Reconstruct the original send [`NotificationTarget`] (all / groups /
/// pcs) from a notification's captured fan-out subjects — the operator's
/// addressing intent, for the SPA's "送信先" display. Groups/pcs are
/// sorted + deduped so the display is stable across replays.
fn parse_notification_target(subjects: &[String]) -> NotificationTarget {
    let mut all = false;
    let mut groups: Vec<String> = Vec::new();
    let mut pcs: Vec<String> = Vec::new();
    for subj in subjects {
        if subj == subject::NOTIFICATIONS_ALL {
            all = true;
        } else if let Some(g) = subj.strip_prefix(subject::NOTIFICATIONS_GROUP_PREFIX) {
            groups.push(g.to_string());
        } else if let Some(pc) = subj.strip_prefix(subject::NOTIFICATIONS_PC_PREFIX) {
            pcs.push(pc.to_string());
        }
    }
    groups.sort();
    groups.dedup();
    pcs.sort();
    pcs.dedup();
    NotificationTarget { all, groups, pcs }
}

fn assemble_roster(
    subjects: &[String],
    agent_rows: &[(String, Option<String>, Option<String>)],
    membership: &HashMap<String, Vec<String>>,
    acks: &[NotificationAckEntry],
) -> Vec<AudiencePc> {
    // Parse the fan-out subjects back into the address triple.
    let mut all = false;
    let mut groups: HashSet<&str> = HashSet::new();
    let mut pcs: HashSet<String> = HashSet::new();
    for subj in subjects {
        if subj == subject::NOTIFICATIONS_ALL {
            all = true;
        } else if let Some(g) = subj.strip_prefix(subject::NOTIFICATIONS_GROUP_PREFIX) {
            groups.insert(g);
        } else if let Some(pc) = subj.strip_prefix(subject::NOTIFICATIONS_PC_PREFIX) {
            pcs.insert(pc.to_string());
        }
    }

    let logon: HashMap<&str, (Option<String>, Option<String>)> = agent_rows
        .iter()
        .map(|(pc, u, d)| (pc.as_str(), (u.clone(), d.clone())))
        .collect();

    // Expand the address triple to the expected PC set.
    let mut expected: HashSet<String> = pcs;
    if all {
        expected.extend(agent_rows.iter().map(|(pc, _, _)| pc.clone()));
    }
    if !groups.is_empty() {
        for (pc_id, pc_groups) in membership {
            if pc_groups.iter().any(|g| groups.contains(g.as_str())) {
                expected.insert(pc_id.clone());
            }
        }
    }

    // Fold acks to PC granularity (confirmed + earliest ack), and make sure
    // every acked PC is in the roster even if it's since fallen out of the
    // resolved audience (a group membership change after the send).
    let mut acked: HashMap<&str, chrono::DateTime<chrono::Utc>> = HashMap::new();
    for a in acks {
        acked
            .entry(a.pc_id.as_str())
            .and_modify(|t| {
                if a.acked_at < *t {
                    *t = a.acked_at;
                }
            })
            .or_insert(a.acked_at);
        expected.insert(a.pc_id.clone());
    }

    // Materialise, sorted pending-first then by pc_id so "who hasn't
    // confirmed" surfaces at the top.
    let mut roster: Vec<AudiencePc> = expected
        .into_iter()
        .map(|pc_id| {
            let acked_at = acked.get(pc_id.as_str()).copied();
            let (last_logon_user, last_logon_display_name) =
                logon.get(pc_id.as_str()).cloned().unwrap_or((None, None));
            AudiencePc {
                last_logon_user,
                last_logon_display_name,
                confirmed: acked_at.is_some(),
                acked_at,
                pc_id,
            }
        })
        .collect();
    roster.sort_by(|a, b| {
        a.confirmed
            .cmp(&b.confirmed)
            .then_with(|| a.pc_id.cmp(&b.pc_id))
    });
    roster
}

/// Drain every retained `notifications.>` message into raw (pre-dedup)
/// `(Notification, subject, stream_seq)` triples, newest-biased via a
/// rolling window. Shared by [`list_sent`] and [`detail`]; callers dedup the
/// per-subject fan-out copies (one publish lands on `all` + each `group.X` +
/// each `pc.Y`) with [`dedup_newest_first`]. The subject is carried so
/// [`detail`] can reconstruct a notification's audience (④) from the very
/// fan-out copies it dedups away — there's no other record of who a
/// notification was addressed to. The stream sequence is carried so
/// [`recall`] can `delete_message` every copy of an id (the only way to
/// physically remove a sent notification from the append-only stream).
async fn replay_all_sent(
    s: &AppState,
) -> Result<Vec<(Notification, String, u64)>, (StatusCode, String)> {
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
        return Ok(Vec::new());
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
    let mut buf: std::collections::VecDeque<(Notification, String, u64)> =
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
            // stop without paying the next fetch's expiry window. The same
            // info carries the stream sequence recall needs to delete the
            // message — read it once.
            let info = m.info();
            if info.as_ref().is_ok_and(|i| i.pending == 0) {
                exhausted = true;
            }
            // Seq 0 never names a real message (JetStream seqs start at 1),
            // so it's a harmless sentinel if info() failed — delete would
            // just no-op, and list/detail ignore the seq entirely.
            let seq = info.as_ref().map(|i| i.stream_sequence).unwrap_or(0);
            match serde_json::from_slice::<Notification>(&m.payload) {
                Ok(n) => {
                    buf.push_back((n, m.subject.to_string(), seq));
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

    Ok(Vec::from(buf))
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
            toast: false,
            issued_at: issued,
            issued_by: None,
            expires_at: None,
            acked_at: None,
            edited_at: None,
            acks_reset_at: None,
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

    // ---- audience roster (assemble_roster pure core) ----

    fn agent(
        pc: &str,
        user: Option<&str>,
        display: Option<&str>,
    ) -> (String, Option<String>, Option<String>) {
        (pc.into(), user.map(Into::into), display.map(Into::into))
    }

    fn ack(pc: &str, sid: &str, secs: i64) -> NotificationAckEntry {
        NotificationAckEntry {
            pc_id: pc.into(),
            user_sid: sid.into(),
            acked_at: at(secs),
            account: None,
        }
    }

    #[test]
    fn parse_target_from_subjects() {
        // group + pc fan-out → the operator's addressing intent.
        let t = parse_notification_target(&[
            "notifications.group.it-admins".to_string(),
            "notifications.pc.minipc".to_string(),
        ]);
        assert!(!t.all);
        assert_eq!(t.groups, vec!["it-admins"]);
        assert_eq!(t.pcs, vec!["minipc"]);

        // `all` is detected; groups/pcs sorted + deduped.
        let t = parse_notification_target(&[
            "notifications.all".to_string(),
            "notifications.group.b".to_string(),
            "notifications.group.a".to_string(),
            "notifications.group.a".to_string(),
        ]);
        assert!(t.all);
        assert_eq!(t.groups, vec!["a", "b"]);
        assert!(t.pcs.is_empty());
    }

    #[test]
    fn roster_all_targets_every_agent_pending_first() {
        // `notifications.all` → every agent PC; only PC2 acked.
        let agents = vec![
            agent("PC1", Some("D\\a"), Some("Alice")),
            agent("PC2", Some("D\\b"), Some("Bob")),
            agent("PC3", None, None),
        ];
        let roster = assemble_roster(
            &["notifications.all".to_string()],
            &agents,
            &HashMap::new(),
            &[ack("PC2", "S-2", 10)],
        );
        let view: Vec<(&str, bool)> = roster
            .iter()
            .map(|r| (r.pc_id.as_str(), r.confirmed))
            .collect();
        // Pending first (PC1, PC3), then confirmed (PC2), each block by pc_id.
        assert_eq!(view, vec![("PC1", false), ("PC3", false), ("PC2", true)]);
        let pc2 = roster.iter().find(|r| r.pc_id == "PC2").unwrap();
        assert_eq!(pc2.acked_at, Some(at(10)));
        assert_eq!(pc2.last_logon_display_name.as_deref(), Some("Bob"));
    }

    #[test]
    fn roster_group_expands_via_membership_and_pc_is_direct() {
        // Target group "fin" + PC9 directly. PC1/PC2 are in "fin".
        let agents = vec![
            agent("PC1", None, None),
            agent("PC2", None, None),
            agent("PC9", None, None),
            agent("PCX", None, None), // not targeted
        ];
        let membership: HashMap<String, Vec<String>> = [
            ("PC1".to_string(), vec!["fin".to_string()]),
            (
                "PC2".to_string(),
                vec!["fin".to_string(), "ops".to_string()],
            ),
            ("PCX".to_string(), vec!["ops".to_string()]),
        ]
        .into_iter()
        .collect();
        let roster = assemble_roster(
            &[
                "notifications.group.fin".to_string(),
                "notifications.pc.PC9".to_string(),
            ],
            &agents,
            &membership,
            &[],
        );
        let mut pcs: Vec<&str> = roster.iter().map(|r| r.pc_id.as_str()).collect();
        pcs.sort();
        assert_eq!(pcs, vec!["PC1", "PC2", "PC9"], "fin members + direct PC9");
        assert!(roster.iter().all(|r| !r.confirmed), "no acks → all pending");
    }

    #[test]
    fn roster_includes_acked_pc_outside_resolved_audience() {
        // PC7 acked but isn't in the targeted group (membership changed
        // after the send) — it must still appear, confirmed.
        let agents = vec![agent("PC1", None, None), agent("PC7", None, None)];
        let membership: HashMap<String, Vec<String>> =
            [("PC1".to_string(), vec!["fin".to_string()])]
                .into_iter()
                .collect();
        let roster = assemble_roster(
            &["notifications.group.fin".to_string()],
            &agents,
            &membership,
            &[ack("PC7", "S-7", 5)],
        );
        let pc7 = roster.iter().find(|r| r.pc_id == "PC7");
        assert!(
            pc7.is_some_and(|r| r.confirmed),
            "acked PC always in roster"
        );
    }

    #[test]
    fn roster_earliest_ack_wins_per_pc() {
        // Two users on PC1 acked at different times → earliest is recorded.
        let agents = vec![agent("PC1", None, None)];
        let roster = assemble_roster(
            &["notifications.pc.PC1".to_string()],
            &agents,
            &HashMap::new(),
            &[ack("PC1", "S-a", 30), ack("PC1", "S-b", 5)],
        );
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].acked_at, Some(at(5)), "earliest ack per PC");
    }
}
