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
    pub confirmed: bool,
    /// Earliest ack instant recorded for this PC; `None` while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<chrono::DateTime<chrono::Utc>>,
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
