//! Agent-wide notification bus (KLP Phase E — live push half).
//!
//! Background task that subscribes to the membership-filtered
//! `notifications.{all|group.X|pc.Y}` core-NATS subjects and
//! re-broadcasts every incoming [`Notification`] onto a process-wide
//! [`tokio::sync::broadcast`] channel. Each KLP connection that has
//! called `notifications.subscribe` holds a `broadcast::Receiver` and
//! forwards what it sees down its own pipe as a `notifications.new`
//! push (see [`crate::klp::handlers::notifications`]).
//!
//! Why one shared subscription + broadcast (not one NATS sub per
//! connection): mirrors the `state.changed` shape — a single agent-wide
//! source fanned out to N per-connection forwarders. It keeps exactly
//! one set of broker subscriptions regardless of how many Client Apps
//! (user sessions) are connected, and a notification published once is
//! delivered to every session, matching SPEC §2.1.5's "セッション単位で
//! ファンアウト".
//!
//! Membership filtering mirrors [`crate::command_replay::filter_subjects`]:
//! narrow per-subject subscriptions (`notifications.all`,
//! `notifications.pc.<id>`, one `notifications.group.<g>` per current
//! membership) rather than a `notifications.>` wildcard, so an agent
//! never receives another PC's / another group's notifications.
//!
//! Delivery is **at-most-once** (SPEC §2.12.7): a notification that
//! arrives while no Client App is connected is dropped here (the
//! broadcast has no receivers). The Client recovers anything it missed
//! via `notifications.list` once it reconnects (history replay lands in
//! a follow-up PR).

use futures::stream::StreamExt;
use kanade_shared::ipc::notifications::{Notification, NotificationAmend};
use kanade_shared::subject;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Bounded depth of the process-wide notification broadcast. Generous
/// for a low-volume channel: notifications are operator-initiated, not
/// telemetry, so even a briefly-stalled forwarder won't lag past this.
/// A slow receiver that does fall behind sees `RecvError::Lagged` and
/// resumes at the oldest still-buffered message (tokio advances the
/// cursor there, not to the newest — handled in the forwarder).
pub const BROADCAST_CAPACITY: usize = 256;

/// The subjects this agent subscribes to for notifications: the
/// fleet-wide `notifications.all`, its own `notifications.pc.<id>`, and
/// one `notifications.group.<g>` per current membership.
///
/// Group subjects are sorted + deduped so the list is a deterministic
/// function of the membership *set* — equal sets compare equal
/// regardless of KV ordering, keeping the unchanged-membership check in
/// [`run`] honest. Mirrors `command_replay::filter_subjects`.
///
/// `pub(crate)` so `notifications.list` reuses the exact same audience →
/// subjects mapping the live bus subscribes to — one source of truth
/// for "which notifications is this agent addressed by".
pub(crate) fn filter_subjects(pc_id: &str, groups: &[String]) -> Vec<String> {
    let mut group_subjects: Vec<String> = groups
        .iter()
        .map(|g| subject::notifications_group(g))
        .collect();
    group_subjects.sort();
    group_subjects.dedup();

    let mut subjects = Vec::with_capacity(2 + group_subjects.len());
    subjects.push(subject::NOTIFICATIONS_ALL.to_string());
    subjects.push(subject::notifications_pc(pc_id));
    subjects.extend(group_subjects);
    subjects
}

/// Spawn the notification bus. `notif_tx` is created by the caller
/// (main.rs) so it can also hand a clone to the KLP `ListenerContext`
/// before this task starts. Returns the detached `JoinHandle`.
pub fn spawn(
    client: async_nats::Client,
    pc_id: String,
    groups_rx: tokio::sync::watch::Receiver<Vec<String>>,
    notif_tx: broadcast::Sender<Notification>,
    amend_tx: broadcast::Sender<NotificationAmend>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(client, pc_id, groups_rx, notif_tx, amend_tx))
}

async fn run(
    client: async_nats::Client,
    pc_id: String,
    mut groups_rx: tokio::sync::watch::Receiver<Vec<String>>,
    notif_tx: broadcast::Sender<Notification>,
    amend_tx: broadcast::Sender<NotificationAmend>,
) {
    // Once the groups task drops its sender (never within a running
    // process, but be defensive), `changed()` resolves immediately and
    // would busy-loop the select — so stop polling that arm. Declared
    // outside the loop: a dropped sender is permanent. Mirrors
    // `command_replay`.
    let mut groups_watch_alive = true;

    loop {
        // Snapshot membership BEFORE subscribing and mark it seen, so a
        // flip racing the subscribe still wakes a later `changed()`
        // rather than being lost.
        let groups: Vec<String> = groups_rx.borrow_and_update().clone();
        let subjects = filter_subjects(&pc_id, &groups);

        // Core subscriptions survive broker reconnects (async-nats
        // re-issues them internally), so we only rebuild on a
        // membership flip — not on disconnect.
        let mut subs = Vec::with_capacity(subjects.len());
        for subj in &subjects {
            match client.subscribe(subj.clone()).await {
                Ok(s) => subs.push(s),
                Err(e) => warn!(error = %e, subject = %subj, "notify_bus: subscribe failed"),
            }
        }
        if subs.is_empty() {
            // Couldn't establish any subscription (broker down at
            // boot). Back off and retry the whole setup.
            warn!(pc_id = %pc_id, "notify_bus: no subscriptions established; retrying");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        if subs.len() < subjects.len() {
            // A 1-of-N transient subscribe failure proceeds with the
            // narrower set rather than retrying — at-most-once + low
            // volume makes a brief gap on one subject acceptable, and
            // the next `groups_rx` flip rebuilds the full set. Log the
            // shortfall so it's visible in the agent log meanwhile.
            warn!(
                pc_id = %pc_id,
                subscribed = subs.len(),
                expected = subjects.len(),
                "notify_bus: partial subscription; some subjects unavailable until next membership change",
            );
        }
        // The amend channel (recall etc.) is a single fleet-wide subject,
        // NOT membership-filtered: a recall is keyed by id and a client
        // ignores ids it doesn't hold, so one subscription reaches everyone.
        // Added to the same `merged` stream and routed by subject in
        // `forward`; deliberately kept out of `filter_subjects` (which
        // `notifications.list` reuses for history replay — `notif-amend`
        // carries no history). Its failure to subscribe is non-fatal (recall
        // just won't live-reflect until the next rebuild).
        match client.subscribe(subject::NOTIFICATIONS_AMEND_SUBJECT).await {
            Ok(s) => subs.push(s),
            Err(e) => warn!(
                error = %e,
                subject = subject::NOTIFICATIONS_AMEND_SUBJECT,
                "notify_bus: amend subscribe failed",
            ),
        }
        let mut merged = futures::stream::select_all(subs);
        info!(
            pc_id = %pc_id,
            groups = ?groups,
            subjects = ?subjects,
            "notify_bus: subscriptions ready",
        );

        loop {
            tokio::select! {
                maybe_msg = merged.next() => {
                    match maybe_msg {
                        Some(msg) => forward(&msg, &notif_tx, &amend_tx, &pc_id),
                        // Every subscriber ended at once — unexpected
                        // for core subs, but rebuild rather than spin.
                        None => {
                            debug!(pc_id = %pc_id, "notify_bus: all subscriptions ended; rebuilding");
                            break;
                        }
                    }
                }
                changed = groups_rx.changed(), if groups_watch_alive => {
                    match changed {
                        Ok(()) => {
                            debug!(pc_id = %pc_id, "notify_bus: membership changed; rebuilding subscriptions");
                            break;
                        }
                        Err(_) => {
                            // Groups sender dropped — keep the current
                            // subscriptions but stop polling this arm.
                            groups_watch_alive = false;
                        }
                    }
                }
            }
        }
    }
}

/// Route one incoming bus message: an amend (recall etc.) on the
/// fleet-wide amend subject, otherwise a [`Notification`] fan-out copy.
/// A `send` error means no Client App is currently subscribed — a
/// normal idle state, not an error, so it's dropped silently (the
/// at-most-once contract; history recovers it via `notifications.list`).
fn forward(
    msg: &async_nats::Message,
    notif_tx: &broadcast::Sender<Notification>,
    amend_tx: &broadcast::Sender<NotificationAmend>,
    pc_id: &str,
) {
    if msg.subject.as_str() == subject::NOTIFICATIONS_AMEND_SUBJECT {
        forward_amend(msg, amend_tx, pc_id);
        return;
    }
    match serde_json::from_slice::<Notification>(&msg.payload) {
        Ok(notification) => {
            let id = notification.id.clone();
            // Capture the toast flag before the value is moved into `send`.
            // Toast surfacing is driven by this flag, NOT by priority.
            let toast = notification.toast;
            match notif_tx.send(notification) {
                Ok(n) => {
                    debug!(pc_id = %pc_id, notification_id = %id, receivers = n, "notify_bus: broadcast");
                    // #647: a toast notification live-pushed while the screen
                    // is locked toasts silently to the Action Center — flag it
                    // so it re-pops on unlock.
                    if toast {
                        super::emergency_notify::note_toast_live_pushed();
                    }
                }
                // `send` returns the un-delivered notification in the
                // error: no Client App is subscribed (the normal idle,
                // at-most-once drop). For a `toast: true` notification that
                // drop would silently lose the whole point, so launch the
                // Client App in the user session to toast it (#102).
                // `toast: false` ones stay dropped — they recover via
                // `notifications.list`.
                Err(broadcast::error::SendError(dropped)) => {
                    if dropped.toast {
                        warn!(
                            pc_id = %pc_id,
                            notification_id = %id,
                            "notify_bus: toast notification with no subscribed client; launching client to toast it",
                        );
                        super::emergency_notify::surface_toast_notification(&id);
                    } else {
                        debug!(
                            pc_id = %pc_id,
                            notification_id = %id,
                            "notify_bus: no connected clients; dropped (recoverable via notifications.list)",
                        );
                    }
                }
            }
        }
        Err(e) => warn!(
            error = %e,
            subject = %msg.subject,
            "notify_bus: failed to decode Notification",
        ),
    }
}

/// Decode one [`NotificationAmend`] off the fleet-wide amend subject and
/// broadcast it to the per-connection forwarders. A `send` error (no Client
/// App subscribed) is dropped silently: an offline client reconciles a recall
/// on reconnect via `notifications.list` (the deleted id no longer appears).
fn forward_amend(
    msg: &async_nats::Message,
    amend_tx: &broadcast::Sender<NotificationAmend>,
    pc_id: &str,
) {
    match serde_json::from_slice::<NotificationAmend>(&msg.payload) {
        Ok(amend) => {
            let id = amend.id.clone();
            match amend_tx.send(amend) {
                Ok(n) => {
                    debug!(pc_id = %pc_id, notification_id = %id, receivers = n, "notify_bus: amend broadcast")
                }
                Err(_) => debug!(
                    pc_id = %pc_id,
                    notification_id = %id,
                    "notify_bus: no connected clients for amend; dropped (recoverable via notifications.list)",
                ),
            }
        }
        Err(e) => warn!(
            error = %e,
            subject = %msg.subject,
            "notify_bus: failed to decode NotificationAmend",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_subjects_includes_all_pc_and_sorted_groups() {
        let subjects = filter_subjects("PC1234", &["wave1".into(), "tokyo".into(), "wave1".into()]);
        assert_eq!(subjects[0], "notifications.all");
        assert_eq!(subjects[1], "notifications.pc.PC1234");
        // groups sorted + deduped after the two fixed subjects.
        assert_eq!(
            &subjects[2..],
            &["notifications.group.tokyo", "notifications.group.wave1"]
        );
    }

    #[test]
    fn filter_subjects_no_groups_is_all_plus_pc() {
        let subjects = filter_subjects("pc-01", &[]);
        assert_eq!(
            subjects,
            vec!["notifications.all", "notifications.pc.pc-01"]
        );
    }
}
