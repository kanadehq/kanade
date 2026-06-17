//! `notifications.*` method handlers (SPEC §2.12.5 — Phase E, live
//! push half).
//!
//! - `notifications.subscribe` — spawn a forwarder task that awaits the
//!   agent-wide notification broadcast ([`crate::klp::notify_bus`]) and
//!   writes a `notifications.new` push onto this connection's `push_tx`
//!   for each incoming notification. Returns the subscription id
//!   (`sub-n-<n>`).
//! - `notifications.unsubscribe` — abort the named forwarder.
//! - `notifications.ack` — write the per-user read mark into the
//!   `notifications_read` KV and publish the
//!   `events.notifications.acked.>` event the backend projects into the
//!   operator's confirmation view.
//! - `notifications.list` — replay the `NOTIFICATIONS` stream filtered
//!   to this agent's audience, annotate each entry with the caller's ack
//!   state, drop expired, and return a paginated newest-first page
//!   (unread/all). The recovery path for pushes missed while the Client
//!   App was disconnected.
//!
//! Mirrors the `state.*` forwarder shape, but the source is a
//! `broadcast::Receiver<Notification>` (discrete events) instead of a
//! `watch::Receiver` (latest-state) — so the forwarder handles
//! `RecvError::Lagged` (a slow client that fell behind; tokio advances
//! the cursor to the oldest still-buffered message, so delivery resumes
//! there and works forward) and `RecvError::Closed` (the bus exited).

use std::collections::HashMap;

use async_nats::jetstream::consumer::pull::Config as PullConfig;
use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use kanade_shared::ipc::envelope::RpcNotification;
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::method;
use kanade_shared::ipc::notifications::{
    Notification, NotificationAcked, NotificationNewParams, NotificationsAckParams,
    NotificationsAckResult, NotificationsFilter, NotificationsListParams, NotificationsListResult,
    NotificationsSubscribeParams, NotificationsSubscribeResult, NotificationsUnsubscribeParams,
};
use kanade_shared::kv::{
    BUCKET_AGENT_GROUPS, BUCKET_NOTIFICATIONS_READ, STREAM_NOTIFICATIONS, notifications_read_key,
    notifications_read_prefix,
};
use kanade_shared::subject;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::super::connection::ConnectionState;
use super::super::notify_bus::filter_subjects;
use super::system::HandlerResult;
use crate::groups::parse_groups;

/// Safety ceiling on how many notifications `notifications.list` replays
/// from the stream in one call. Notifications are operator-broadcast
/// (not telemetry), so a 90-day history is realistically dozens — this
/// cap only guards against a runaway. If a fleet ever exceeds it the
/// overflow is logged (never silently dropped) and the oldest beyond
/// the cap are omitted.
const MAX_REPLAY: usize = 5000;

/// Per-fetch batch size when draining the stream.
const REPLAY_BATCH: usize = 500;

/// Hard upper bound on `limit`, mirroring the wire doc on
/// [`NotificationsListParams::limit`]. A hand-rolled client asking for
/// more is clamped rather than allowed to pull unbounded history.
const MAX_LIMIT: usize = 200;

/// `notifications.subscribe` — start streaming `notifications.new`
/// pushes for this connection. Derives a fresh broadcast receiver from
/// the agent-wide bus and registers the forwarder so
/// `notifications.unsubscribe` can abort it.
pub fn handle_notifications_subscribe(
    conn: &mut ConnectionState,
    _params: NotificationsSubscribeParams,
) -> HandlerResult<NotificationsSubscribeResult> {
    let rx = conn.notif_subscribe().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "notification bus not available on this agent build",
        )
    })?;
    let push_tx = conn.push_tx.clone();
    let pc_id = conn.pc_id.clone();
    let handle = tokio::spawn(forward_notifications(rx, push_tx, pc_id));
    let id = conn.subscriptions.register("n", handle);
    Ok(NotificationsSubscribeResult { subscription: id })
}

/// `notifications.unsubscribe` — abort the named forwarder. Returns
/// [`ErrorKind::NotFound`] when the id doesn't match a live
/// subscription.
pub fn handle_notifications_unsubscribe(
    conn: &mut ConnectionState,
    params: NotificationsUnsubscribeParams,
) -> HandlerResult<()> {
    if conn.subscriptions.unsubscribe(&params.subscription) {
        Ok(())
    } else {
        Err(RpcError::new(
            ErrorKind::NotFound,
            format!("subscription '{}' not found", params.subscription),
        ))
    }
}

/// `notifications.ack` — record the caller's confirmation of one
/// notification (SPEC §2.12.4 / Phase E). Two side effects:
///
/// 1. Write the per-user read mark into the `notifications_read` KV
///    under `{pc_id}.{user_sid}.{notification_id}`, so
///    `notifications.list` can filter this user's unread set.
/// 2. Publish `events.notifications.acked.{pc_id}.{user_sid}.{notif_id}`
///    (an acknowledged JetStream publish) so the backend's
///    notification-acks projector records who confirmed when — that
///    feeds `GET /api/notifications/{id}/ack_status`.
///
/// The SID is the OS-derived [`ConnectionState::peer`] identity, never
/// a payload field (SPEC §2.12.4): a user can only ack as themselves,
/// even on a shared PC. A connection whose SID couldn't be resolved
/// (`"<unknown>"`) is rejected rather than writing a colliding row.
pub async fn handle_notifications_ack(
    conn: &ConnectionState,
    params: NotificationsAckParams,
) -> HandlerResult<NotificationsAckResult> {
    // Validate inputs before touching NATS so a bad request fails
    // cheaply (and so the guard paths are unit-testable without a
    // broker).
    let user_sid = conn.peer.user_sid.as_str();
    if user_sid.is_empty() || user_sid == "<unknown>" {
        // Identity problem, not a server fault, and not retryable —
        // Unauthorized rather than InternalError (matches the same
        // guard in notifications.list).
        return Err(RpcError::new(
            ErrorKind::Unauthorized,
            "caller SID could not be resolved; cannot record ack",
        ));
    }
    let notif_id = params.id.trim();
    if !valid_notification_id(notif_id) {
        // The id flows into a NATS KV key and the ack publish subject,
        // so an unvalidated id with NATS-special chars (space, `.`
        // beyond the allowed set, wildcards `*` / `>`, `/`) would be
        // rejected by the broker and surface as an opaque
        // InternalError. Reject up front with InvalidParams instead.
        return Err(RpcError::new(
            ErrorKind::InvalidParams,
            "notification id must be non-empty and contain only [A-Za-z0-9_.-]",
        ));
    }
    let client = conn.nats.as_ref().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "NATS client not available on this agent build",
        )
    })?;
    let pc_id = conn.pc_id.as_str();
    let acked_at = Utc::now();

    let js = async_nats::jetstream::new(client.clone());

    // 1. Persist the per-user read mark. Value matches SPEC §2.3.2:
    //    `{"acked_at": ..., "acked_by": "<sid>"}`.
    let kv = js
        .get_key_value(BUCKET_NOTIFICATIONS_READ)
        .await
        .map_err(|e| {
            RpcError::new(
                ErrorKind::InternalError,
                format!("open {BUCKET_NOTIFICATIONS_READ} KV: {e}"),
            )
        })?;
    let key = notifications_read_key(pc_id, user_sid, notif_id);
    let value = serde_json::to_vec(&serde_json::json!({
        "acked_at": acked_at,
        "acked_by": user_sid,
    }))
    .map_err(|e| RpcError::new(ErrorKind::InternalError, e.to_string()))?;
    kv.put(key, value.into()).await.map_err(|e| {
        RpcError::new(
            ErrorKind::InternalError,
            format!("write {BUCKET_NOTIFICATIONS_READ}: {e}"),
        )
    })?;

    // 2. Publish the ack event (acknowledged JetStream publish so a
    //    broker problem surfaces here instead of silently dropping the
    //    operator's confirmation view).
    // The login name (`DOMAIN\sam`) the agent resolved for this
    // connection's peer — a far more legible label than the SID in the
    // operator's confirmation view. Omit it when unresolved so the
    // backend's PC-last-logon fallback takes over instead of recording a
    // useless `<unknown>`.
    let account = {
        let u = conn.peer.user.as_str();
        (!u.is_empty() && u != "<unknown>").then(|| u.to_string())
    };
    let event = NotificationAcked {
        notification_id: notif_id.to_string(),
        pc_id: pc_id.to_string(),
        user_sid: user_sid.to_string(),
        acked_at,
        account,
    };
    let payload = serde_json::to_vec(&event)
        .map_err(|e| RpcError::new(ErrorKind::InternalError, e.to_string()))?;
    let subj = subject::events_notifications_acked(pc_id, user_sid, notif_id);
    let ack = js
        .publish(subj.clone(), payload.into())
        .await
        .map_err(|e| RpcError::new(ErrorKind::InternalError, format!("publish {subj}: {e}")))?;
    ack.await.map_err(|e| {
        RpcError::new(
            ErrorKind::InternalError,
            format!("ack publish to {subj} not confirmed: {e}"),
        )
    })?;

    info!(
        pc_id = %pc_id,
        user_sid = %user_sid,
        notification_id = %notif_id,
        "notification acked",
    );
    Ok(NotificationsAckResult { acked_at })
}

/// `notifications.list` — paginated history of the notifications this
/// agent is addressed by (SPEC §2.12.5 / Phase E). Replays the
/// `NOTIFICATIONS` stream filtered to this PC's audience (`all` +
/// `pc.<id>` + each `group.<g>`, the exact subjects the live bus
/// subscribes to), annotates each notification with the caller's ack
/// state from `notifications_read`, drops expired ones, and returns the
/// requested page newest-first.
///
/// `filter = Unread` (the Client App's default first paint) returns only
/// the notifications this user hasn't acked; `All` returns the full
/// window. This is a cold, user-initiated path (opening the panel), so
/// the stream replay + KV scan per call is fine — no cached snapshot, so
/// a just-sent notification shows up on the next call without restart.
pub async fn handle_notifications_list(
    conn: &ConnectionState,
    params: NotificationsListParams,
) -> HandlerResult<NotificationsListResult> {
    let client = conn.nats.as_ref().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "notifications.list: NATS client not wired into the connection",
        )
    })?;
    let user_sid = conn.peer.user_sid.as_str();
    if user_sid.is_empty() || user_sid == "<unknown>" {
        // An unresolved caller SID is an identity problem, not a
        // server fault — and it won't fix itself on retry, so signal
        // Unauthorized (non-retryable) rather than InternalError.
        return Err(RpcError::new(
            ErrorKind::Unauthorized,
            "notifications.list: caller SID could not be resolved",
        ));
    }
    let js = async_nats::jetstream::new(client.clone());

    let my_groups = read_my_groups(&js, &conn.pc_id).await?;
    let subjects = filter_subjects(&conn.pc_id, &my_groups);
    let notifications = replay_notifications(&js, subjects).await?;
    let acks = read_user_acks(&js, &conn.pc_id, user_sid).await?;

    let limit = (params.limit as usize).clamp(1, MAX_LIMIT);
    let offset = decode_cursor(params.cursor.as_deref());
    Ok(build_notifications_list(
        notifications,
        &acks,
        params.filter,
        Utc::now(),
        limit,
        offset,
    ))
}

/// Read this PC's group membership from `BUCKET_AGENT_GROUPS` (mirrors
/// `maintenance.list`). A missing entry → no groups (not an error); a
/// broker-level failure propagates so the client retries rather than
/// silently missing every group-targeted notification.
async fn read_my_groups(
    js: &async_nats::jetstream::Context,
    pc_id: &str,
) -> HandlerResult<Vec<String>> {
    let kv = js.get_key_value(BUCKET_AGENT_GROUPS).await.map_err(|e| {
        warn!(error = %e, "notifications.list: open BUCKET_AGENT_GROUPS failed");
        RpcError::new(
            ErrorKind::InternalError,
            format!("notifications.list: open group membership: {e}"),
        )
    })?;
    match kv.get(pc_id).await {
        Ok(Some(bytes)) => Ok(parse_groups(&bytes)),
        Ok(None) => Ok(Vec::new()),
        Err(e) => {
            warn!(error = %e, "notifications.list: agent_groups read failed");
            Err(RpcError::new(
                ErrorKind::InternalError,
                format!("notifications.list: read group membership: {e}"),
            ))
        }
    }
}

/// Drain the `NOTIFICATIONS` stream for the given audience subjects via
/// a throwaway ephemeral, read-only (`AckPolicy::None`) pull consumer.
/// `fetch` returns immediately with whatever is retained (no waiting for
/// new messages), so the loop ends when a fetch yields nothing.
/// Unparseable payloads are skipped (logged). Capped at [`MAX_REPLAY`]
/// with a warn — never a silent truncation.
async fn replay_notifications(
    js: &async_nats::jetstream::Context,
    subjects: Vec<String>,
) -> HandlerResult<Vec<Notification>> {
    let stream = js.get_stream(STREAM_NOTIFICATIONS).await.map_err(|e| {
        warn!(error = %e, "notifications.list: get_stream NOTIFICATIONS failed");
        RpcError::new(
            ErrorKind::InternalError,
            format!("notifications.list: open stream: {e}"),
        )
    })?;
    let consumer = stream
        .create_consumer(PullConfig {
            deliver_policy: DeliverPolicy::All,
            ack_policy: AckPolicy::None,
            filter_subjects: subjects,
            // Reap the throwaway consumer shortly after this call ends.
            inactive_threshold: std::time::Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .map_err(|e| {
            warn!(error = %e, "notifications.list: create ephemeral consumer failed");
            RpcError::new(
                ErrorKind::InternalError,
                format!("notifications.list: create consumer: {e}"),
            )
        })?;

    // `DeliverPolicy::All` delivers oldest→newest. Keep a rolling
    // window of the newest [`MAX_REPLAY`] (drop from the front as it
    // overflows) so that if the stream ever exceeds the cap the page
    // still surfaces the *freshest* notifications — the ones an
    // unread-first UI cares about — rather than a wall of stale history.
    let mut buf: std::collections::VecDeque<Notification> =
        std::collections::VecDeque::with_capacity(REPLAY_BATCH.min(MAX_REPLAY));
    let mut dropped = 0usize;
    loop {
        let mut batch = consumer
            .fetch()
            .max_messages(REPLAY_BATCH)
            // Without an explicit expiry, the pull request blocks for
            // the server default (~5 s) once the stream is drained
            // (fewer than max available) — a latency penalty on every
            // call. Retained messages deliver near-instantly, so a short
            // window is ample; the loop just pays it once on the tail.
            .expires(std::time::Duration::from_millis(200))
            .messages()
            .await
            .map_err(|e| {
                warn!(error = %e, "notifications.list: fetch failed");
                RpcError::new(
                    ErrorKind::InternalError,
                    format!("notifications.list: fetch: {e}"),
                )
            })?;
        let mut got = 0usize;
        while let Some(m) = batch.next().await {
            let m = m.map_err(|e| {
                RpcError::new(
                    ErrorKind::InternalError,
                    format!("notifications.list: message: {e}"),
                )
            })?;
            got += 1;
            match serde_json::from_slice::<Notification>(&m.payload) {
                Ok(n) => {
                    buf.push_back(n);
                    if buf.len() > MAX_REPLAY {
                        buf.pop_front();
                        dropped += 1;
                    }
                }
                Err(e) => warn!(
                    error = %e,
                    subject = %m.subject,
                    "notifications.list: skipping unparseable notification",
                ),
            }
        }
        // A short (< full) batch means the stream is drained — stop
        // rather than pay another expiry window on a guaranteed-empty
        // fetch.
        if got < REPLAY_BATCH {
            break;
        }
    }
    if dropped > 0 {
        warn!(
            dropped,
            cap = MAX_REPLAY,
            "notifications.list: stream exceeded replay cap; oldest beyond the cap omitted",
        );
    }
    Ok(Vec::from(buf))
}

/// Value side of a `notifications_read` KV row — the agent writes
/// `{"acked_at": …, "acked_by": …}` (see `handle_notifications_ack`); we
/// only need `acked_at` back to annotate the history.
#[derive(Deserialize)]
struct ReadMark {
    acked_at: DateTime<Utc>,
}

/// Read this `(pc_id, user_sid)`'s ack state from `notifications_read`
/// into a `notification_id → acked_at` map.
///
/// `notifications_read` is a single **fleet-wide** KV bucket, so `kv.keys()`
/// would stream every PC's + every user's ack key just to filter in memory,
/// then issue an N+1 `get` per match — O(fleet) work per call. Instead, scan
/// the bucket's backing JetStream stream (`KV_notifications_read`) through a
/// throwaway ephemeral, read-only consumer filtered to **just this user's**
/// subject space — `$KV.notifications_read.{pc_id}.{user_sid}.>` — the same
/// ephemeral-consumer pattern [`replay_notifications`] uses. (Filtering on the
/// JetStream subject is also what sidesteps the async-nats KV key validation
/// that rejects a `>` wildcard passed to `kv.history()` — the original bug
/// this change fixes.) The bucket is `history: 1`, so each key yields its
/// single latest revision; a KV delete/purge arrives as an empty-payload
/// tombstone, handled below.
async fn read_user_acks(
    js: &async_nats::jetstream::Context,
    pc_id: &str,
    user_sid: &str,
) -> HandlerResult<HashMap<String, DateTime<Utc>>> {
    let stream_name = format!("KV_{BUCKET_NOTIFICATIONS_READ}");
    let stream = js.get_stream(&stream_name).await.map_err(|e| {
        warn!(error = %e, %stream_name, "notifications.list: get_stream for read state failed");
        RpcError::new(
            ErrorKind::InternalError,
            format!("notifications.list: open read state stream: {e}"),
        )
    })?;
    let prefix = notifications_read_prefix(pc_id, user_sid);
    let filter = format!("$KV.{BUCKET_NOTIFICATIONS_READ}.{prefix}>");
    let consumer = stream
        .create_consumer(PullConfig {
            deliver_policy: DeliverPolicy::All,
            ack_policy: AckPolicy::None,
            filter_subjects: vec![filter],
            // Reap the throwaway consumer shortly after this call ends.
            inactive_threshold: std::time::Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .map_err(|e| {
            warn!(error = %e, "notifications.list: create read-state consumer failed");
            RpcError::new(
                ErrorKind::InternalError,
                format!("notifications.list: create read state consumer: {e}"),
            )
        })?;

    // Subject → notification id: strip the `$KV.<bucket>.{pc}.{sid}.` prefix;
    // the remainder is the id (the same suffix `notifications_read_key`
    // appends).
    let subject_prefix = format!("$KV.{BUCKET_NOTIFICATIONS_READ}.{prefix}");
    let mut out = HashMap::new();
    loop {
        let mut batch = consumer
            .fetch()
            .max_messages(REPLAY_BATCH)
            .expires(std::time::Duration::from_millis(200))
            .messages()
            .await
            .map_err(|e| {
                warn!(error = %e, "notifications.list: fetch read state failed");
                RpcError::new(
                    ErrorKind::InternalError,
                    format!("notifications.list: fetch read state: {e}"),
                )
            })?;
        let mut got = 0usize;
        while let Some(m) = batch.next().await {
            let m = m.map_err(|e| {
                RpcError::new(
                    ErrorKind::InternalError,
                    format!("notifications.list: read state message: {e}"),
                )
            })?;
            got += 1;
            let Some(id) = m.subject.as_str().strip_prefix(&subject_prefix) else {
                continue;
            };
            // A KV delete/purge is an empty-payload tombstone — the ack was
            // cleared, so drop any earlier value for this id.
            if m.payload.is_empty() {
                out.remove(id);
                continue;
            }
            match serde_json::from_slice::<ReadMark>(&m.payload) {
                Ok(mark) => {
                    out.insert(id.to_string(), mark.acked_at);
                }
                Err(e) => warn!(
                    subject = %m.subject,
                    error = %e,
                    "notifications.list: skipping unparseable read mark",
                ),
            }
        }
        // A short (< full) batch means the stream is drained.
        if got < REPLAY_BATCH {
            break;
        }
    }
    Ok(out)
}

/// Decode the opaque pagination cursor into a row offset. A cursor is
/// just the next offset as a decimal string (see
/// [`build_notifications_list`]); anything unparseable restarts from 0.
fn decode_cursor(cursor: Option<&str>) -> usize {
    cursor.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0)
}

/// Pure core: raw replayed notifications + the caller's ack map → one
/// page of the `notifications.list` result. Annotates `acked_at`, drops
/// expired (SPEC: the Client App stops surfacing past expiry), applies
/// the unread/all filter, sorts newest-first, and slices `[offset,
/// offset+limit)`. `next_cursor` is the next offset when more remain.
///
/// Offset-based pagination is pragmatic for this low-volume, cold path:
/// a notification arriving mid-pagination could shift the offset by one,
/// but the unread panel is typically a single page and the cost of a
/// rare duplicate/skip across pages is negligible versus a seq cursor's
/// complexity.
fn build_notifications_list(
    notifications: Vec<Notification>,
    acks: &HashMap<String, DateTime<Utc>>,
    filter: NotificationsFilter,
    now: DateTime<Utc>,
    limit: usize,
    offset: usize,
) -> NotificationsListResult {
    // Annotate ack state, drop expired, and dedup by id (a well-behaved
    // publish emits each id once, but a bad one could repeat it — keep
    // the newest issued_at).
    let mut idx_of: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<Notification> = Vec::new();
    for mut n in notifications {
        // Past expiry → never surfaced (SPEC: the Client App stops
        // showing it). `is_some_and` keeps this a plain stable Option
        // check (no let-chain) and reads cleanly.
        if n.expires_at.is_some_and(|exp| exp <= now) {
            continue;
        }
        n.acked_at = acks.get(&n.id).copied();
        match idx_of.get(&n.id) {
            Some(&i) if n.issued_at <= deduped[i].issued_at => {}
            Some(&i) => deduped[i] = n,
            None => {
                idx_of.insert(n.id.clone(), deduped.len());
                deduped.push(n);
            }
        }
    }

    let mut items: Vec<Notification> = match filter {
        NotificationsFilter::Unread => deduped
            .into_iter()
            .filter(|n| n.acked_at.is_none())
            .collect(),
        NotificationsFilter::All => deduped,
    };
    // Newest first; id breaks ties so equal-instant entries are stable.
    items.sort_by(|a, b| b.issued_at.cmp(&a.issued_at).then_with(|| a.id.cmp(&b.id)));

    let total = items.len();
    let page: Vec<Notification> = items.into_iter().skip(offset).take(limit).collect();
    let next_offset = offset + page.len();
    let next_cursor = (next_offset < total).then(|| next_offset.to_string());
    NotificationsListResult {
        items: page,
        next_cursor,
    }
}

/// `notifications.ack` id charset gate. Same `[A-Za-z0-9_.-]` set as
/// `jobs::valid_job_id` (kept local so the two namespaces stay
/// decoupled) — these are the characters safe in both a NATS KV key
/// and a publish subject token, so a bad id is caught as
/// `InvalidParams` here rather than as an opaque broker error later.
fn valid_notification_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Forwarder task body. Awaits each broadcast notification, builds a
/// `notifications.new` push, and sends it on `push_tx`. Exits when the
/// connection's writer is gone (`push_tx` closed) or the bus shut down
/// (`Closed`). On `Lagged` (only reachable after a >256-deep backlog,
/// implausible for operator-initiated notifications) tokio drops the
/// missed span and advances to the oldest still-buffered message; the
/// loop logs the skip and resumes delivery from there.
async fn forward_notifications(
    mut rx: broadcast::Receiver<Notification>,
    push_tx: mpsc::Sender<Vec<u8>>,
    pc_id: String,
) {
    debug!(pc_id = %pc_id, "notifications forwarder: subscribed");
    loop {
        let notification = match rx.recv().await {
            Ok(n) => n,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    pc_id = %pc_id,
                    skipped,
                    "notifications forwarder: lagged; resuming at oldest buffered",
                );
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!(pc_id = %pc_id, "notifications forwarder: bus closed, exiting");
                return;
            }
        };
        let params = NotificationNewParams { notification };
        let notif = match RpcNotification::new(method::NOTIFICATIONS_NEW, &params) {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "notifications forwarder: failed to encode notification");
                continue;
            }
        };
        let body = match serde_json::to_vec(&notif) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "notifications forwarder: failed to serialise frame");
                continue;
            }
        };
        if push_tx.send(body).await.is_err() {
            debug!(pc_id = %pc_id, "notifications forwarder: push channel closed, exiting");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klp::auth::PeerCredentials;
    use kanade_shared::ipc::envelope::RpcMessage;
    use kanade_shared::ipc::notifications::NotificationPriority;
    use kanade_shared::ipc::state::StateSnapshot;
    use kanade_shared::wire::EffectiveConfig;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::watch;

    fn dummy_snapshot() -> StateSnapshot {
        StateSnapshot {
            pc_id: "PC1234".into(),
            online: true,
            vpn: "unknown".into(),
            checks: vec![],
            agent_version: "0.43.0".into(),
            target_version: "0.43.0".into(),
        }
    }

    fn sample_notification(id: &str) -> Notification {
        Notification {
            id: id.into(),
            priority: NotificationPriority::Emergency,
            require_ack: true,
            title: "緊急: ネットワーク機器メンテ".into(),
            body: "22時から30分停止します".into(),
            issued_at: chrono::Utc::now(),
            issued_by: Some("infra-team".into()),
            expires_at: None,
            acked_at: None,
        }
    }

    fn fresh_conn(
        notif_tx: &broadcast::Sender<Notification>,
        push_tx: mpsc::Sender<Vec<u8>>,
    ) -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let (_state_tx, state_rx) = watch::channel(dummy_snapshot());
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                user_sid: "S-1-5-21-1001".into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.43.0".into(),
            cfg_rx,
            state_rx,
            PathBuf::from("agent.log"),
            push_tx,
        )
        .with_notifications(notif_tx.clone())
    }

    #[tokio::test]
    async fn subscribe_returns_sub_n_id_and_registers_forwarder() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, _push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let r1 = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();
        let r2 = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();
        assert_eq!(r1.subscription, "sub-n-1");
        assert_eq!(r2.subscription, "sub-n-2");
        assert_eq!(conn.subscriptions.len(), 2);
    }

    #[tokio::test]
    async fn subscribed_forwarder_pushes_notifications_new() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, mut push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let _ = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();

        notif_tx.send(sample_notification("notif-9f3a")).unwrap();

        let body = tokio::time::timeout(Duration::from_secs(1), push_rx.recv())
            .await
            .expect("forwarder should push within 1s")
            .expect("push_tx still open");
        let msg: RpcMessage = serde_json::from_slice(&body).expect("decode frame");
        match msg {
            RpcMessage::Notification(n) => {
                assert_eq!(n.method, method::NOTIFICATIONS_NEW);
                let params: NotificationNewParams =
                    serde_json::from_value(n.params).expect("decode NotificationNewParams");
                assert_eq!(params.notification.id, "notif-9f3a");
                assert_eq!(
                    params.notification.priority,
                    NotificationPriority::Emergency
                );
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unsubscribe_aborts_forwarder() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, mut push_rx) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let r = handle_notifications_subscribe(&mut conn, NotificationsSubscribeParams::default())
            .unwrap();
        assert_eq!(conn.subscriptions.len(), 1);

        handle_notifications_unsubscribe(
            &mut conn,
            NotificationsUnsubscribeParams {
                subscription: r.subscription,
            },
        )
        .expect("unsubscribe should succeed");
        assert_eq!(conn.subscriptions.len(), 0);

        // After unsubscribe a broadcast must not push.
        notif_tx.send(sample_notification("notif-2")).unwrap();
        let res = tokio::time::timeout(Duration::from_millis(200), push_rx.recv()).await;
        assert!(res.is_err(), "expected no push after unsubscribe");
    }

    #[tokio::test]
    async fn unsubscribe_unknown_id_returns_not_found() {
        let (notif_tx, _) = broadcast::channel(8);
        let (push_tx, _) = mpsc::channel(8);
        let mut conn = fresh_conn(&notif_tx, push_tx);
        let err = handle_notifications_unsubscribe(
            &mut conn,
            NotificationsUnsubscribeParams {
                subscription: "sub-n-999".into(),
            },
        )
        .expect_err("unknown id must error");
        assert_eq!(err.data.expect("data").kind, ErrorKind::NotFound);
    }

    /// Build a connection with an explicit SID and no NATS client, for
    /// exercising the `notifications.ack` input guards (which run before
    /// any broker access). The happy path needs a live broker and is
    /// covered by integration tests, not here.
    fn conn_for_ack(user_sid: &str) -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let (_state_tx, state_rx) = watch::channel(dummy_snapshot());
        let (push_tx, _push_rx) = mpsc::channel(8);
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                user_sid: user_sid.into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.43.0".into(),
            cfg_rx,
            state_rx,
            PathBuf::from("agent.log"),
            push_tx,
        )
    }

    #[test]
    fn valid_notification_id_accepts_ids_rejects_nats_unsafe() {
        for ok in ["notif-9f3a", "maintenance-2026-05-20", "a.b", "Job_123"] {
            assert!(valid_notification_id(ok), "{ok} should be valid");
        }
        for bad in ["", "has space", "wild*", "a>b", "with/slash", "qu?x"] {
            assert!(!valid_notification_id(bad), "{bad:?} should be invalid");
        }
    }

    #[tokio::test]
    async fn ack_blank_or_unsafe_id_returns_invalid_params() {
        let conn = conn_for_ack("S-1-5-21-1001");
        for bad in ["  ", "bad id", "wild*"] {
            let err = handle_notifications_ack(&conn, NotificationsAckParams { id: bad.into() })
                .await
                .expect_err("bad id must error");
            assert_eq!(
                err.data.expect("data").kind,
                ErrorKind::InvalidParams,
                "id {bad:?}",
            );
        }
    }

    #[tokio::test]
    async fn ack_unknown_sid_is_rejected() {
        // A connection whose SID couldn't be resolved must not write a
        // colliding `<unknown>` KV row.
        let conn = conn_for_ack("<unknown>");
        let err = handle_notifications_ack(
            &conn,
            NotificationsAckParams {
                id: "notif-1".into(),
            },
        )
        .await
        .expect_err("unknown SID must error");
        let data = err.data.expect("data");
        assert_eq!(data.kind, ErrorKind::Unauthorized);
        assert!(data.detail.contains("SID"), "detail: {}", data.detail);
    }

    #[tokio::test]
    async fn ack_without_nats_client_errors_internal() {
        // Valid SID + id, but the test connection has no NATS client
        // (conn_for_ack skips with_nats) — the handler reports an
        // internal error rather than panicking.
        let conn = conn_for_ack("S-1-5-21-1001");
        let err = handle_notifications_ack(
            &conn,
            NotificationsAckParams {
                id: "notif-1".into(),
            },
        )
        .await
        .expect_err("missing NATS client must error");
        assert_eq!(err.data.expect("data").kind, ErrorKind::InternalError);
    }

    // ---- notifications.list pure core ----

    fn list_base() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn notif_at(id: &str, issued: DateTime<Utc>, expires: Option<DateTime<Utc>>) -> Notification {
        Notification {
            id: id.into(),
            priority: NotificationPriority::Info,
            require_ack: false,
            title: "t".into(),
            body: "b".into(),
            issued_at: issued,
            issued_by: None,
            expires_at: expires,
            acked_at: None,
        }
    }

    #[test]
    fn decode_cursor_parses_offset_or_zero() {
        assert_eq!(decode_cursor(None), 0);
        assert_eq!(decode_cursor(Some("25")), 25);
        assert_eq!(decode_cursor(Some("garbage")), 0);
    }

    #[test]
    fn unread_filter_excludes_acked_and_keeps_unacked() {
        let base = list_base();
        let items = vec![
            notif_at("a", base, None),
            notif_at("b", base + chrono::Duration::seconds(60), None),
        ];
        let mut acks = HashMap::new();
        acks.insert("a".to_string(), base + chrono::Duration::seconds(120));
        let r = build_notifications_list(items, &acks, NotificationsFilter::Unread, base, 50, 0);
        assert_eq!(r.items.len(), 1, "only the unacked notification remains");
        assert_eq!(r.items[0].id, "b");
        assert!(r.items[0].acked_at.is_none());
        assert!(r.next_cursor.is_none());
    }

    #[test]
    fn all_filter_includes_acked_with_acked_at_annotated() {
        let base = list_base();
        let items = vec![notif_at("a", base, None)];
        let mut acks = HashMap::new();
        let when = base + chrono::Duration::seconds(120);
        acks.insert("a".to_string(), when);
        let r = build_notifications_list(items, &acks, NotificationsFilter::All, base, 50, 0);
        assert_eq!(r.items.len(), 1);
        assert_eq!(
            r.items[0].acked_at,
            Some(when),
            "ack state annotated for history"
        );
    }

    #[test]
    fn drops_expired_in_both_filters() {
        let base = list_base();
        let past = base - chrono::Duration::seconds(60);
        let items = vec![
            notif_at("live", base, Some(base + chrono::Duration::seconds(3600))),
            notif_at("dead", base, Some(past)),
        ];
        for filter in [NotificationsFilter::Unread, NotificationsFilter::All] {
            let r = build_notifications_list(items.clone(), &HashMap::new(), filter, base, 50, 0);
            let ids: Vec<&str> = r.items.iter().map(|n| n.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["live"],
                "expired notification dropped ({filter:?})"
            );
        }
    }

    #[test]
    fn newest_first_and_offset_pagination() {
        let base = list_base();
        let items = vec![
            notif_at("oldest", base, None),
            notif_at("mid", base + chrono::Duration::seconds(60), None),
            notif_at("newest", base + chrono::Duration::seconds(120), None),
        ];
        // Page 1: limit 2 → newest two, cursor points past them.
        let p1 = build_notifications_list(
            items.clone(),
            &HashMap::new(),
            NotificationsFilter::All,
            base,
            2,
            0,
        );
        let ids1: Vec<&str> = p1.items.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids1, vec!["newest", "mid"], "newest first");
        assert_eq!(p1.next_cursor.as_deref(), Some("2"));
        // Page 2: from the cursor offset → the tail, no further cursor.
        let p2 = build_notifications_list(
            items,
            &HashMap::new(),
            NotificationsFilter::All,
            base,
            2,
            decode_cursor(p1.next_cursor.as_deref()),
        );
        let ids2: Vec<&str> = p2.items.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids2, vec!["oldest"]);
        assert!(p2.next_cursor.is_none(), "tail page has no next cursor");
    }

    #[test]
    fn dedups_by_id_keeping_newest_issued() {
        let base = list_base();
        let items = vec![
            notif_at("dup", base, None),
            notif_at("dup", base + chrono::Duration::seconds(60), None),
        ];
        let r = build_notifications_list(
            items,
            &HashMap::new(),
            NotificationsFilter::All,
            base,
            50,
            0,
        );
        assert_eq!(r.items.len(), 1, "same id collapses to one");
        assert_eq!(r.items[0].issued_at, base + chrono::Duration::seconds(60));
    }
}
