//! `notifications.*` method types — paginated history + ack +
//! push for incoming notifications.
//!
//! The notification lifecycle (SPEC §2.12.8 emergency example):
//!
//! 1. Operator publishes via backend HTTP API → backend writes to
//!    NATS `NOTIFICATIONS` JetStream.
//! 2. Agent consumes the stream, fans out to connected clients via
//!    `notifications.new` push.
//! 3. User clicks "確認" → client sends `notifications.ack` → agent
//!    writes `notifications_read` KV (keyed by
//!    `{pc_id}.{user_sid}.{notification_id}`) AND publishes
//!    `events.notifications.acked.{pc_id}.{user_sid}.{notification_id}`
//!    so the SPA can show per-user confirmation status.
//! 4. Past notifications stay queryable via `notifications.list` —
//!    that's the recovery path when the agent missed a push during
//!    a network blip.

use serde::{Deserialize, Serialize};

// ---------- shared notification body ----------

/// Notification body — used both for [`NotificationsListResult`]
/// entries and the [`NotificationNewParams`] push.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct Notification {
    /// Stable id minted by the backend (UUID v7). Identifies the
    /// notification for ack / history lookups.
    pub id: String,
    pub priority: NotificationPriority,
    /// Whether the user must explicitly click "確認" to dismiss.
    /// Non-acked notifications stay pinned on the Client App's
    /// notification panel until clicked; acked ones drop into
    /// history.
    #[serde(default)]
    pub require_ack: bool,
    pub title: String,
    pub body: String,
    /// Whether to surface an OS toast for this notification — decoupled
    /// from [`priority`](Self::priority). `true` gives the full "make
    /// sure they see it" treatment (persistent native toast; the agent
    /// launches the Client App when it isn't running; lands in the lock
    /// screen / Action Center; re-pops on logon/unlock). `false` shows it
    /// only in the in-app list. `#[serde(default)]` (⇒ `false`) just so a
    /// pre-this-field body on the retained stream still decodes — it is
    /// NOT a priority fallback; toast behaviour is driven solely by this
    /// flag.
    #[serde(default)]
    pub toast: bool,
    /// When the notification was created (backend wall clock).
    pub issued_at: chrono::DateTime<chrono::Utc>,
    /// Optional human-readable label of who created the
    /// notification (e.g. `"infra-team"` in SPEC §2.12.8). Surfaced
    /// in the Client App for context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<String>,
    /// Optional expiry (SPEC §2.4.1 `expires_at`). Past this instant
    /// the Client App stops surfacing the notification (it drops out
    /// of toasts / the modal / the unread badge) even if never acked.
    /// `None` ⇒ the notification never auto-expires. Additive +
    /// optional so pre-Phase-E bodies on the wire still decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `acked_at` from this user's perspective. Populated by
    /// `notifications.list` for already-acked entries; never set on
    /// `notifications.new` pushes (a fresh push by definition
    /// hasn't been acked yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When this notification was last edited (`PATCH /api/notifications/{id}`),
    /// re-published with the same `id` + `issued_at` but new content. `None`
    /// ⇒ never edited. Lets the SPA show an "edited" badge and lets a client
    /// recognise a re-published copy as a content update of one it already
    /// holds (vs a fresh arrival). Additive + optional so pre-edit bodies on
    /// the retained stream still decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When an edit reset confirmations: any ack (read mark) recorded *before*
    /// this instant is stale and the user must re-confirm the new content.
    /// The agent's `notifications.list` treats a read mark older than this as
    /// unread; a connected client clears a locally-held ack older than this on
    /// the live update. `None` ⇒ acks were never reset (the common case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acks_reset_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Severity ladder. Drives the SPA color, toast/dialog choice, and
/// whether the Client App grabs window focus on push arrival.
/// `#[non_exhaustive]` so a future SPEC can add severities (e.g.
/// `Critical` above Emergency) without a wire bump.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NotificationPriority {
    /// Background-style toast. Routine maintenance reminders.
    Info,
    /// Yellow toast. Heads-up about upcoming changes.
    Warn,
    /// Red modal — grabs window focus, blocks until ack
    /// (SPEC §2.12.8: "緊急: ネットワーク機器メンテ").
    Emergency,
    /// #492: serde-level forward-compat catch-all. `#[non_exhaustive]`
    /// only affects Rust match exhaustiveness — serde still hard-fails
    /// on an unknown variant STRING, so a newer peer's new variant
    /// used to make older readers reject the whole containing message.
    /// Unknown decodes any unrecognised value; UIs render it neutrally.
    #[serde(other)]
    Unknown,
}

// ---------- notifications.list ----------

/// `notifications.list` params — paginated history of notifications
/// this user has received (per-user, scoped via OS SID).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsListParams {
    /// Filter: which subset of the user's notifications to return.
    /// Defaults to [`NotificationsFilter::Unread`] — the Client App
    /// loads the unread bucket on first paint.
    #[serde(default)]
    pub filter: NotificationsFilter,
    /// Max number of entries to return. Clamped agent-side to a
    /// safe upper bound (currently 200) so a misbehaving client
    /// can't ask for unbounded history. Defaults to 50.
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Continuation token from a prior response's
    /// [`NotificationsListResult::next_cursor`]. `None` on first
    /// page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl Default for NotificationsListParams {
    fn default() -> Self {
        Self {
            filter: NotificationsFilter::default(),
            limit: default_limit(),
            cursor: None,
        }
    }
}

fn default_limit() -> u32 {
    50
}

/// History-list filter selector.
#[derive(
    Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum NotificationsFilter {
    /// Only entries this user has NOT acked. Default — the Client
    /// App's notification panel opens to this view.
    #[default]
    Unread,
    /// Everything in the user's history window, acked or not.
    All,
}

/// `notifications.list` response.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsListResult {
    pub items: Vec<Notification>,
    /// Opaque continuation token. `Some(cursor)` ⇒ caller should
    /// re-request with `params.cursor = Some(cursor)` to fetch the
    /// next page; `None` ⇒ caller has the tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ---------- notifications.subscribe ----------

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct NotificationsSubscribeParams {}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsSubscribeResult {
    pub subscription: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsUnsubscribeParams {
    pub subscription: String,
}

// ---------- notifications.new (push) ----------

/// Push payload for `notifications.new`. The full notification body
/// inline — no second round-trip needed.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationNewParams {
    #[serde(flatten)]
    pub notification: Notification,
}

// ---------- notifications.ack ----------

/// `notifications.ack` params — mark this notification read for the
/// caller's user (SID derived from the OS at connect time, NOT
/// from the payload). SPEC §2.12.4 forbids ack-ing other users'
/// notifications even on a shared PC — the agent rejects with
/// `Unauthorized` if the notification's audience doesn't include
/// the caller.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsAckParams {
    pub id: String,
}

/// `notifications.ack` response — confirms the agent persisted the
/// ack and published the `events.notifications.acked.>` event.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsAckResult {
    /// Wall-clock the agent wrote into `notifications_read` KV.
    pub acked_at: chrono::DateTime<chrono::Utc>,
}

// ---------- notifications.unack ----------

/// `notifications.unack` params — retract this user's prior ack (the
/// read↔unread toggle): the user clicked "確認" by mistake and wants the
/// notification back as unread. Same SID-from-the-OS / audience guard as
/// [`NotificationsAckParams`]; a user may only unack their own
/// confirmation.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsUnackParams {
    pub id: String,
}

/// `notifications.unack` response — confirms the agent deleted the
/// `notifications_read` KV entry and published
/// `events.notifications.unacked.>`. Carries the instant the revoke was
/// recorded (the agent's wall clock), so the operator's audit view can
/// show "confirmed at X, retracted at Y".
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationsUnackResult {
    pub unacked_at: chrono::DateTime<chrono::Utc>,
}

// ---------- backend HTTP compose (POST /api/notifications) ----------

/// Operator-facing request body for `POST /api/notifications` (and the
/// equivalent `notifications/*.yaml` manifest, SPEC §2.4.1). The
/// backend mints the [`Notification::id`] (when `id` is omitted) and
/// [`Notification::issued_at`], resolves [`target`](Self::target) into
/// the `notifications.{all|group.X|pc.Y}` fan-out subjects, and
/// publishes one [`Notification`] per resolved subject into the
/// `NOTIFICATIONS` stream.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct PublishNotificationRequest {
    /// Operator-supplied id — the manifest's `id:` doubles as the
    /// notification id (SPEC §2.4.1). Omit it for ad-hoc SPA composer
    /// sends and the backend mints a UUID instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub priority: NotificationPriority,
    #[serde(default)]
    pub require_ack: bool,
    pub title: String,
    pub body: String,
    /// Surface an OS toast (see [`Notification::toast`]). Decoupled from
    /// `priority`; defaults to `false` (in-app only).
    #[serde(default)]
    pub toast: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Fan-out audience — same shape as a job manifest's `target:`
    /// (SPEC §2.4.1). At least one of `all` / `groups` / `pcs` must be
    /// set or the backend rejects the request.
    pub target: crate::manifest::Target,
}

/// Response of `POST /api/notifications` — the minted/echoed id plus
/// the subjects the notification fanned out to, so the operator UI can
/// confirm the resolved audience.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct PublishNotificationResponse {
    pub id: String,
    pub subjects: Vec<String>,
}

// ---------- backend HTTP edit (PATCH /api/notifications/{id}) --------

/// Operator-facing request body for `PATCH /api/notifications/{id}` — edit
/// an already-sent notification's content (fix a typo, shorten/extend the
/// expiry, change priority / require_ack / toast) without re-sending it.
///
/// The **audience is immutable** here — there is no `target` field. Changing
/// who it goes to is "recall → re-send" (the backend keeps the original
/// fan-out subjects). `id`, `issued_at`, and `issued_by` are preserved; only
/// the fields below change. The backend deletes the old stream copies and
/// re-publishes the merged notification under the same id + `issued_at` (so
/// "sent at" is unchanged), stamping [`Notification::edited_at`].
///
/// Unlike [`PublishNotificationRequest`] this is a *full* edit set (the SPA
/// pre-fills every field from the current notification and submits them all),
/// so there is no per-field optionality to disambiguate; `expires_at: None`
/// means "never expires", a past instant expires it immediately.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct EditNotificationRequest {
    pub priority: NotificationPriority,
    #[serde(default)]
    pub require_ack: bool,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub toast: bool,
    /// `None` ⇒ never expires; a past instant expires it immediately (unlike
    /// `publish`, which rejects a past expiry as a likely typo — here it is a
    /// deliberate "retire it but keep history" choice, distinct from recall).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Reset confirmations: when `true` the backend clears every recorded ack
    /// for this notification and stamps [`Notification::acks_reset_at`], so a
    /// materially-changed body forces everyone to re-confirm. `false` (the
    /// default, e.g. a typo fix) leaves existing confirmations intact.
    #[serde(default)]
    pub reset_acks: bool,
}

// ---------- ack event (Agent → NATS → backend projector) ----------

/// Body of the
/// `events.notifications.acked.{pc_id}.{user_sid}.{notif_id}` event the
/// agent publishes when a user acks a notification. The backend's
/// notification-acks projector reads these fields from the JSON body
/// (not by parsing the subject) so an id / SID containing a `.` can't
/// desync the projected row from its subject tokens.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationAcked {
    pub notification_id: String,
    pub pc_id: String,
    pub user_sid: String,
    pub acked_at: chrono::DateTime<chrono::Utc>,
    /// The acking user's login name (`DOMAIN\sam` or `.\user`), from the
    /// agent connection's resolved peer identity — far more legible than
    /// the raw SID in the operator's confirmation view. Additive +
    /// optional so a pre-this-version agent's ack (SID only) still
    /// decodes; the backend falls back to the PC's last-logon identity
    /// when it's absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

// ---------- unack event (Agent → NATS → backend projector) --------

/// Body of the
/// `events.notifications.unacked.{pc_id}.{user_sid}.{notif_id}` event the
/// agent publishes when a user *retracts* a confirmation. Mirror of
/// [`NotificationAcked`]; the projector reads these body fields (not the
/// subject) and, in the same stream-ordered consumer, appends a
/// `kind = 'unacked'` row to `notification_ack_events` and stamps
/// `notification_acks.unacked_at` so the SPA roster flips the recipient
/// from confirmed back to "未確認" while the audit log keeps the original
/// ack.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationUnacked {
    pub notification_id: String,
    pub pc_id: String,
    pub user_sid: String,
    pub unacked_at: chrono::DateTime<chrono::Utc>,
    /// The retracting user's login name — same provenance and fallback
    /// semantics as [`NotificationAcked::account`]. Carried for audit
    /// symmetry (the projector's DELETE/UPDATE keys on the SID, not the
    /// account).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

// ---------- ack status (GET /api/notifications/{id}/ack_status) ----

/// One recipient's confirmation record for a notification.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationAckEntry {
    pub pc_id: String,
    pub user_sid: String,
    pub acked_at: chrono::DateTime<chrono::Utc>,
    /// Human-readable label for who confirmed — the acking user's login
    /// name from the ack event, or (for pre-account acks) the PC's
    /// last-logon display name / login as a fallback. `None` only when
    /// neither is available, in which case the SPA shows the `user_sid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// When this user *retracted* their confirmation (the read↔unread
    /// toggle). `Some` ⇒ they confirmed at `acked_at` then later took it
    /// back at this instant — the SPA renders this recipient as "取消済み"
    /// (confirmed→revoked), distinct from both "確認済み" and a
    /// never-confirmed "未確認". `None` ⇒ the confirmation still stands.
    /// Additive + optional so a pre-unack backend's `ack_status` still
    /// decodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unacked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Response of `GET /api/notifications/{id}/ack_status` — every
/// `(pc_id, user_sid, acked_at)` tuple recorded for the notification,
/// powering the SPA's "who confirmed when" view.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationAckStatus {
    pub id: String,
    pub acks: Vec<NotificationAckEntry>,
}

// ---------- detail (GET /api/notifications/{id}) ------------------

/// Response of `GET /api/notifications/{id}` — one sent notification's
/// full content (so the SPA can show "what was sent", including the
/// `body` the history table truncates away) paired with its
/// per-recipient confirmation list. Powers the deep-linkable
/// `/notifications/{id}` detail page, which an operator opens in a new
/// tab from the history list (Ctrl/⌘ click), mirroring the Activity →
/// result-detail deep link.
///
/// `acks` is the same set `ack_status` returns; bundling it here saves
/// the detail page a second round-trip.
///
/// `audience` is the per-PC confirmation roster (④): the set of PCs the
/// notification was addressed to, each flagged confirmed/pending, so an
/// operator can see *who hasn't* acknowledged — not just who has. Empty
/// when the audience couldn't be reconstructed (e.g. the fan-out subjects
/// aged out of the stream).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationDetail {
    pub notification: Notification,
    pub acks: Vec<NotificationAckEntry>,
    #[serde(default)]
    pub audience: Vec<AudiencePc>,
    /// The original send target (where it was addressed: all / groups /
    /// pcs), reconstructed from the fan-out subjects — so the SPA can show
    /// "送信先" (vs `audience`, which is the *resolved* per-PC roster).
    /// `None` when the subjects couldn't be reconstructed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<NotificationTarget>,
}

/// The audience a notification was *addressed* to (the `target:` of the
/// publish), reconstructed from its fan-out subjects
/// (`notifications.{all|group.X|pc.Y}`). Distinct from the resolved
/// per-PC [`AudiencePc`] roster: this is the operator's intent ("sent to
/// the it-admins group + PC minipc"), not the expanded PC list.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationTarget {
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub pcs: Vec<String>,
}

/// One targeted PC's confirmation state, for the detail page's "who
/// hasn't confirmed" roster (④). Resolved by expanding the notification's
/// fan-out subjects (`all` / `group.X` / `pc.Y`) to the fleet's PCs and
/// joining against the recorded acks.
///
/// Granularity is the PC, not the individual user: the backend has no
/// full per-PC user roster, only each host's last-logon identity, so
/// `last_logon_*` stands in as "the PC's representative user". `confirmed`
/// is true when *any* user on that PC acked (the detailed who-and-when is
/// in `acks`).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct AudiencePc {
    pub pc_id: String,
    /// The host's last sign-in account (`DOMAIN\sam`) / display name from
    /// the `agents` row — `None` for a targeted PC with no agent record
    /// (e.g. an explicit `pc.Y` target that never registered).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_logon_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_logon_display_name: Option<String>,
    /// `true` when this PC currently has a *standing* confirmation — at
    /// least one user acked and has not since retracted it. A PC whose
    /// only ack was later revoked is `confirmed = false` with
    /// `unacked_at = Some` (the "取消済み" state), so the operator's
    /// "who hasn't confirmed" roster counts it as not-confirmed while
    /// still surfacing that it once was.
    pub confirmed: bool,
    /// Earliest ack instant recorded for this PC; `None` while pending.
    /// Retained even after a revoke so the audit view can show
    /// "confirmed at X → retracted at Y".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When this PC's confirmation was retracted (the latest revoke
    /// across its users). `Some` with `confirmed = false` ⇒ "取消済み"
    /// (was confirmed, then taken back); `None` ⇒ never retracted (either
    /// still confirmed or never confirmed — disambiguated by `confirmed`
    /// / `acked_at`). Additive + optional for pre-unack decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unacked_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ---------- amend (post-send operations) -------------------------

/// A post-send amendment to an already-fanned-out notification, broadcast
/// fleet-wide on the ephemeral [`crate::subject::NOTIFICATIONS_AMEND_SUBJECT`]
/// channel so every connected client showing the notification can react in
/// real time. Carries only the notification `id` plus the operation — a
/// client applies it only if it currently holds that id (an id it never
/// received is a no-op), so the single broadcast needs no audience routing.
///
/// The durable half of an operation lives in the backend (recall deletes the
/// stream copies; a future edit re-publishes them); this is the "update the
/// screens that are showing it right now" half. Built to grow: today only
/// `Recall`, but `op` is a tagged enum so an `Update`/`SetExpiry` variant can
/// be added without breaking the wire format.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct NotificationAmend {
    pub id: String,
    pub op: NotificationAmendOp,
}

/// The operation an [`NotificationAmend`] applies. Tagged on `kind` so future
/// data-carrying variants (e.g. `Update { notification }`) stay wire-compatible.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationAmendOp {
    /// The notification was recalled (deleted): remove it from the panel,
    /// unread badge, and any open require-ack modal.
    Recall,
}

/// Params of the `notifications.amended` push (Agent → Client) — the
/// flattened [`NotificationAmend`] (`{ "id", "kind": "recall" }`).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct NotificationAmendedParams {
    #[serde(flatten)]
    pub amend: NotificationAmend,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn priority_serialises_snake_case() {
        for (variant, expected) in [
            (NotificationPriority::Info, "\"info\""),
            (NotificationPriority::Warn, "\"warn\""),
            (NotificationPriority::Emergency, "\"emergency\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "encode {variant:?}");
            let back: NotificationPriority = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant, "round-trip {expected}");
        }
    }

    #[test]
    fn filter_defaults_to_unread() {
        // The Client App's notification panel opens to "unread" so
        // the default selector must match.
        let p = NotificationsListParams::default();
        assert_eq!(p.filter, NotificationsFilter::Unread);
        // Default decode of an empty object.
        let p: NotificationsListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.filter, NotificationsFilter::Unread);
        assert_eq!(p.limit, 50);
    }

    #[test]
    fn notification_new_spec_example_decodes() {
        // SPEC §2.12.8's emergency push payload, verbatim. The
        // flatten attribute means the wire is the Notification's
        // own keys at the top level — no `notification: {…}` nest.
        let wire = r#"{
            "id":"notif-9f3a","priority":"emergency","require_ack":true,
            "title":"緊急: ネットワーク機器メンテ","body":"22時から30分停止します",
            "issued_at":"2026-05-20T12:00:00Z","issued_by":"infra-team"
        }"#;
        let p: NotificationNewParams = serde_json::from_str(wire).expect("decode");
        assert_eq!(p.notification.id, "notif-9f3a");
        assert_eq!(p.notification.priority, NotificationPriority::Emergency);
        assert!(p.notification.require_ack);
        assert_eq!(p.notification.title, "緊急: ネットワーク機器メンテ");
        assert_eq!(p.notification.issued_by.as_deref(), Some("infra-team"));
    }

    #[test]
    fn notification_expires_at_is_optional_and_skipped_when_none() {
        // Additive field: a body without expires_at decodes (None) and
        // a None value is omitted from the wire so pre-Phase-E
        // consumers don't see a null key.
        let wire = r#"{
            "id":"n1","priority":"info","title":"t","body":"b",
            "issued_at":"2026-05-20T12:00:00Z"
        }"#;
        let n: Notification = serde_json::from_str(wire).expect("decode without expires_at");
        assert!(n.expires_at.is_none());
        let v = serde_json::to_value(&n).unwrap();
        assert!(
            v.get("expires_at").is_none(),
            "None expires_at omitted: {v:?}"
        );
    }

    #[test]
    fn notification_toast_defaults_false_and_round_trips() {
        // A body on the retained stream from before the `toast` field
        // decodes with toast = false (so old messages just don't toast).
        let wire = r#"{
            "id":"n1","priority":"info","title":"t","body":"b",
            "issued_at":"2026-05-20T12:00:00Z"
        }"#;
        let n: Notification = serde_json::from_str(wire).expect("decode without toast");
        assert!(!n.toast, "absent toast ⇒ false (in-app only, not a toast)");

        // And an explicit toast:true round-trips.
        let wire_true = r#"{
            "id":"n2","priority":"warn","title":"t","body":"b","toast":true,
            "issued_at":"2026-05-20T12:00:00Z"
        }"#;
        let n: Notification = serde_json::from_str(wire_true).expect("decode toast:true");
        assert!(n.toast);
        // Decoupled from priority: a warn can carry toast:true.
        assert_eq!(n.priority, NotificationPriority::Warn);
    }

    #[test]
    fn publish_request_toast_defaults_false_and_decodes() {
        // Toast is driven ONLY by this flag (decoupled from priority by
        // design): an omitted `toast` decodes to false even for an
        // emergency — the caller must opt in with `toast: true`. There is
        // deliberately no priority fallback.
        let req: PublishNotificationRequest =
            serde_json::from_str(r#"{"priority":"emergency","title":"t","body":"b","target":{}}"#)
                .expect("decode without toast");
        assert!(!req.toast, "omitted toast ⇒ false, even for emergency");

        let req: PublishNotificationRequest = serde_json::from_str(
            r#"{"priority":"warn","title":"t","body":"b","toast":true,"target":{}}"#,
        )
        .expect("decode with toast:true");
        assert!(req.toast, "explicit toast:true on a non-emergency priority");
    }

    #[test]
    fn publish_request_requires_target_audience() {
        // The wire decodes a target with no audience set; the handler
        // is what rejects it. Here we just pin Target::is_specified so
        // the handler's guard has a stable contract to lean on.
        let req: PublishNotificationRequest =
            serde_json::from_str(r#"{"priority":"warn","title":"t","body":"b","target":{}}"#)
                .expect("decode");
        assert!(!req.target.is_specified(), "empty target is unspecified");
        assert_eq!(req.id, None, "id omitted ⇒ backend mints one");
        assert!(!req.require_ack, "require_ack defaults false");
        assert!(!req.toast, "toast defaults false");
    }

    #[test]
    fn edit_request_decodes_with_defaults() {
        // Minimal body: the SPA always submits all editable fields, but
        // require_ack / toast / reset_acks default false and expires_at omitted
        // ⇒ never expires.
        let req: EditNotificationRequest =
            serde_json::from_str(r#"{"priority":"warn","title":"t","body":"b"}"#).expect("decode");
        assert!(!req.require_ack);
        assert!(!req.toast);
        assert!(
            !req.reset_acks,
            "reset_acks defaults false (keep confirmations)"
        );
        assert_eq!(req.expires_at, None, "omitted expiry ⇒ never expires");

        // reset_acks + an explicit expiry decode as set.
        let req: EditNotificationRequest = serde_json::from_str(
            r#"{"priority":"info","title":"t","body":"b","reset_acks":true,"expires_at":"2099-01-01T00:00:00Z"}"#,
        )
        .expect("decode");
        assert!(req.reset_acks);
        assert!(req.expires_at.is_some());
    }

    #[test]
    fn notification_edit_fields_default_none_and_round_trip() {
        // A pre-edit body (no edited_at / acks_reset_at) still decodes, and
        // both fields are omitted on the wire when None.
        let n: Notification = serde_json::from_str(
            r#"{"id":"n1","priority":"info","title":"t","body":"b","issued_at":"2026-06-01T00:00:00Z"}"#,
        )
        .expect("decode pre-edit body");
        assert_eq!(n.edited_at, None);
        assert_eq!(n.acks_reset_at, None);
        let v = serde_json::to_value(&n).unwrap();
        assert!(
            v.get("edited_at").is_none(),
            "None edited_at omitted: {v:?}"
        );
        assert!(
            v.get("acks_reset_at").is_none(),
            "None acks_reset_at omitted: {v:?}"
        );
    }

    #[test]
    fn notification_acked_round_trips() {
        let t = chrono::Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 5).unwrap();
        let a = NotificationAcked {
            notification_id: "notif-9f3a".into(),
            pc_id: "PC1234".into(),
            // SIDs use hyphens, never dots — safe alongside the dotted
            // subject, but the projector reads this body field anyway.
            user_sid: "S-1-5-21-1001".into(),
            acked_at: t,
            account: Some("EXAMPLE\\taro".into()),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: NotificationAcked = serde_json::from_str(&json).unwrap();
        assert_eq!(back.notification_id, a.notification_id);
        assert_eq!(back.pc_id, a.pc_id);
        assert_eq!(back.user_sid, a.user_sid);
        assert_eq!(back.acked_at, t);
        assert_eq!(back.account.as_deref(), Some("EXAMPLE\\taro"));
    }

    #[test]
    fn notification_amend_recall_round_trips() {
        // Wire shape the backend broadcasts and the client decodes:
        // the op is tagged on `kind` so adding a data-carrying variant
        // later (Update { .. }) stays compatible.
        let a = NotificationAmend {
            id: "notif-9f3a".into(),
            op: NotificationAmendOp::Recall,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["id"], "notif-9f3a");
        assert_eq!(v["op"]["kind"], "recall");
        let back: NotificationAmend = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);

        // The push params flatten the amend (no nested "amend" key).
        let p = NotificationAmendedParams { amend: a.clone() };
        let pv = serde_json::to_value(&p).unwrap();
        assert_eq!(pv["id"], "notif-9f3a");
        assert_eq!(pv["op"]["kind"], "recall");
        assert!(pv.get("amend").is_none(), "amend is flattened: {pv:?}");
    }

    #[test]
    fn notification_acked_without_account_decodes() {
        // A pre-account agent emits the ack body without `account`; it must
        // still decode (None), and a None account is omitted on the wire so
        // older readers never see a null key.
        let wire = r#"{
            "notification_id":"n1","pc_id":"PC1","user_sid":"S-1-5-21-1",
            "acked_at":"2026-05-20T12:00:05Z"
        }"#;
        let a: NotificationAcked = serde_json::from_str(wire).expect("decode without account");
        assert_eq!(a.account, None);
        let v = serde_json::to_value(&a).unwrap();
        assert!(v.get("account").is_none(), "None account omitted: {v:?}");
    }

    #[test]
    fn ack_result_round_trips() {
        let t = chrono::Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 5).unwrap();
        let r = NotificationsAckResult { acked_at: t };
        let json = serde_json::to_string(&r).unwrap();
        let back: NotificationsAckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.acked_at, t);
    }

    #[test]
    fn unack_result_round_trips() {
        let t = chrono::Utc.with_ymd_and_hms(2026, 5, 20, 12, 5, 0).unwrap();
        let r = NotificationsUnackResult { unacked_at: t };
        let json = serde_json::to_string(&r).unwrap();
        let back: NotificationsUnackResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.unacked_at, t);
    }

    #[test]
    fn notification_unacked_round_trips_and_account_optional() {
        let t = chrono::Utc.with_ymd_and_hms(2026, 5, 20, 12, 5, 0).unwrap();
        let u = NotificationUnacked {
            notification_id: "notif-9f3a".into(),
            pc_id: "PC1234".into(),
            user_sid: "S-1-5-21-1001".into(),
            unacked_at: t,
            account: Some("EXAMPLE\\taro".into()),
        };
        let json = serde_json::to_string(&u).unwrap();
        let back: NotificationUnacked = serde_json::from_str(&json).unwrap();
        assert_eq!(back.notification_id, u.notification_id);
        assert_eq!(back.pc_id, u.pc_id);
        assert_eq!(back.user_sid, u.user_sid);
        assert_eq!(back.unacked_at, t);
        assert_eq!(back.account.as_deref(), Some("EXAMPLE\\taro"));

        // account omitted ⇒ decodes None and is left off the wire.
        let wire = r#"{
            "notification_id":"n1","pc_id":"PC1","user_sid":"S-1-5-21-1",
            "unacked_at":"2026-05-20T12:05:00Z"
        }"#;
        let u: NotificationUnacked = serde_json::from_str(wire).expect("decode without account");
        assert_eq!(u.account, None);
        let v = serde_json::to_value(&u).unwrap();
        assert!(v.get("account").is_none(), "None account omitted: {v:?}");
    }

    #[test]
    fn ack_entry_unacked_at_optional_and_skipped_when_none() {
        // A pre-unack backend emits an ack entry with no unacked_at; it
        // must decode (None) and a None value is omitted on the wire.
        let wire = r#"{
            "pc_id":"PC1","user_sid":"S-1-5-21-1","acked_at":"2026-05-20T12:00:05Z"
        }"#;
        let e: NotificationAckEntry =
            serde_json::from_str(wire).expect("decode without unacked_at");
        assert_eq!(e.unacked_at, None);
        let v = serde_json::to_value(&e).unwrap();
        assert!(
            v.get("unacked_at").is_none(),
            "None unacked_at omitted: {v:?}"
        );
    }

    #[test]
    fn notifications_list_paginates_via_cursor() {
        // First page: no cursor.
        let p = NotificationsListParams {
            filter: NotificationsFilter::All,
            limit: 25,
            cursor: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("cursor").is_none(), "wire: {v:?}");

        // Continuation: cursor present.
        let p = NotificationsListParams {
            cursor: Some("opaque-token".into()),
            ..NotificationsListParams::default()
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["cursor"], "opaque-token");
    }
}
