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
