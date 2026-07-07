use serde::{Deserialize, Serialize};

use crate::config::MailSection;

/// Upper bound on [`ServerSettings::agent_prune_days`] (100 years). A
/// value this large already means "effectively never", and it keeps the
/// cleanup task's `now - Duration::days(n)` subtraction comfortably inside
/// `chrono::DateTime`'s representable range — `DateTime - Duration` panics
/// on overflow, and an unbounded `u32` (~11.7 M years) would trip it.
/// Enforced two ways: the PUT handler rejects a larger value, and
/// [`ServerSettings::effective_agent_prune_days`] clamps to it so even a
/// hand-written KV value can never panic the cleanup task.
pub const MAX_AGENT_PRUNE_DAYS: u32 = 36_500;

/// Built-in retention window (days) for the `collections` Object Store —
/// the file bundles a `collect:` job uploads (#219). This is the value the
/// bootstrap creates a fresh bucket with, and the fallback
/// [`ServerSettings::effective_collect_retention_days`] resolves to when the
/// operator hasn't overridden it. Kept here (not just in `bootstrap`) so the
/// wire default and the bucket's initial `max_age` share one source of truth.
pub const DEFAULT_COLLECT_RETENTION_DAYS: u32 = 30;

/// Upper bound on [`ServerSettings::collect_retention_days`] (10 years).
/// Bundles are debugging / audit artifacts, so an operator has no reason to
/// keep them longer than this; the ceiling stops a fat-fingered value from
/// pushing the `collections` Object Store's `max_age` to something absurd.
/// (The bucket's `max_bytes` cap still bounds actual disk regardless.)
/// Enforced by the PUT handler and clamped in
/// [`ServerSettings::effective_collect_retention_days`] so even a
/// hand-written KV value stays sane.
pub const MAX_COLLECT_RETENTION_DAYS: u32 = 3650;

/// Built-in default for [`ServerSettings::session_ttl_hours`] — how long a
/// freshly-minted SPA/CLI login token stays valid when the operator hasn't
/// overridden it. 24h balances "don't re-auth mid-workday" against a
/// bounded window for a leaked token (the DB row is re-checked every
/// request, so `disable` still takes effect immediately regardless).
pub const DEFAULT_SESSION_TTL_HOURS: u32 = 24;

/// Upper bound on [`ServerSettings::session_ttl_hours`] (365 days). Keeps
/// login's `Utc::now() + Duration::hours(n)` comfortably inside `chrono`'s
/// representable range (its nanosecond `i64` caps out ~292 years, so an
/// unclamped `u32` of hours could overflow) and stops an operator pinning a
/// practically-immortal session. Enforced by the PUT handler and clamped in
/// [`ServerSettings::effective_session_ttl_hours`] so even a hand-written
/// KV value stays bounded.
pub const MAX_SESSION_TTL_HOURS: u32 = 8_760;

/// Value stored in the `server_settings` KV bucket under the single key
/// [`crate::kv::KEY_SERVER_SETTINGS`]. Operator-editable, backend-side
/// server configuration that isn't per-agent (so it doesn't belong in
/// `agent_config`'s layered scopes) and isn't a fleet-wide switch every
/// agent watches (so it doesn't belong in `fleet_config`). Managed via
/// the SPA Settings page's "server settings" tab.
///
/// Every field is `Option<_>`: `None` (the default / the JSON value
/// `null` / the field simply absent) means **unset — fall back to the
/// built-in default** ([`ServerSettings::defaults`]), exactly like the
/// agent layered-config scopes. The SPA renders the built-in default as a
/// faint placeholder so a blank field shows what it resolves to, and when
/// a real default is introduced here it appears in the UI (and takes
/// effect for already-deployed-but-unset fleets) for free.
///
/// `#[serde(default)]` on the container keeps the document backward/forward
/// compatible: a freshly-created or missing key decodes to all-`None`
/// (pre-feature behaviour), an older backend reading a newer document
/// ignores unknown fields, and a newer backend reading an older document
/// fills the missing field with `None`. Keep that invariant — never add a
/// field whose `None` doesn't mean "behave as before".
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ServerSettings {
    /// Days a dead agent (one whose heartbeat stopped arriving) may
    /// linger in the `agents` registry before the backend cleanup task
    /// prunes its row.
    ///
    /// `None` (unset) falls back to the built-in default; with no default
    /// configured that resolves to pruning **disabled** (see
    /// [`ServerSettings::effective_agent_prune_days`]). A positive value
    /// makes the cleanup sweep delete rows whose `last_heartbeat` is older
    /// than that many days. The `agents` table is a projection of the
    /// heartbeat stream, so a machine that's merely offline (not gone)
    /// reappears on its next heartbeat (~30s cadence); only
    /// genuinely-retired machines stay gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_prune_days: Option<u32>,

    /// Agent group whose members are the trusted **controller-tier**
    /// runners. A job with `tier: controller` (e.g. a `feed:` job that
    /// fetches an external URL) is dispatched ONLY to members of this group;
    /// `None` (unset) means controller-tier jobs run **nowhere** (fail-safe,
    /// so an external fetch never lands on an employee endpoint by accident).
    /// See `crate::manifest::Tier`. `None` ⇒ no controller runners
    /// configured, which is the safe default for a fresh deployment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller_group: Option<String>,

    /// Non-secret SMTP relay settings for outbound email (compliance-alert
    /// notifications, account setup links, …). Lives here (#884) rather than
    /// in `backend.toml` so an operator can edit it from the SPA without
    /// shell access to the host.
    ///
    /// `None` (unset) ⇒ no relay configured, email is a no-op — the in-app
    /// / NATS notification path is unaffected. The SMTP **password is
    /// deliberately not here** (KV is
    /// readable over NATS): it stays sourced from the `MailPassword`
    /// registry secret / `$KANADE_MAIL_PASSWORD` and is combined with these
    /// settings when the backend builds its `Mailer`. Changes take effect on
    /// the **next backend restart** (the backend builds the `Mailer` once at
    /// startup — no live rebuild), which is acceptable per the #884
    /// discussion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail: Option<MailSection>,

    /// Retention window (days) for collected file bundles — the `collect:`
    /// job archives uploaded to the `collections` Object Store (#219).
    ///
    /// Unlike the other fields, this one has a **real built-in default**
    /// ([`DEFAULT_COLLECT_RETENTION_DAYS`], 30 d): `None` (unset) falls back
    /// to it, so a blank field preserves the historical behaviour rather than
    /// disabling retention. A positive value tells the backend to reconcile
    /// the Object Store's `max_age` to that many days (see
    /// `bootstrap::reconcile_collect_retention`), applied at boot and again
    /// whenever this document is saved from the SPA. Clamped to
    /// [`MAX_COLLECT_RETENTION_DAYS`]. The bucket's `max_bytes` cap is
    /// untouched, so extending the window can't make the store grow unbounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collect_retention_days: Option<u32>,

    /// How many hours a freshly-minted login token (SPA / CLI) stays valid
    /// before the caller must re-authenticate. Read backend-side by the
    /// login handler when it mints the JWT `exp`; changing it affects
    /// **tokens minted after the change**, not already-issued ones.
    ///
    /// Like [`collect_retention_days`](Self::collect_retention_days) this
    /// carries a **real built-in default** ([`DEFAULT_SESSION_TTL_HOURS`],
    /// 24h): `None` (unset) falls back to it, so a blank field resolves to a
    /// usable window rather than an instantly-expired token. Clamped to
    /// `1..=`[`MAX_SESSION_TTL_HOURS`]. The SPA renders `24` as a faint
    /// placeholder on a blank field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_ttl_hours: Option<u32>,
}

impl ServerSettings {
    /// Built-in defaults applied when a field is unset (`None`) in the
    /// stored document. Most fields default to `None` (no fleet-meaningful
    /// default — a blank prune window means "disabled" rather than some
    /// arbitrary number of days). The exceptions carry real defaults so a
    /// blank field still resolves to a sensible value:
    /// [`collect_retention_days`](Self::collect_retention_days)
    /// ([`DEFAULT_COLLECT_RETENTION_DAYS`], preserving the historical 30-day
    /// retention) and [`session_ttl_hours`](Self::session_ttl_hours)
    /// ([`DEFAULT_SESSION_TTL_HOURS`], 24h).
    ///
    /// Exposed via `GET /api/server-settings/defaults` so the SPA renders
    /// these as faint placeholders (mirroring the agent layered-config
    /// page's built-in floor). Introducing a real default is a one-line
    /// change here that automatically shows up in the UI and applies to
    /// every deployment that hasn't overridden the field.
    pub fn defaults() -> Self {
        Self {
            agent_prune_days: None,
            controller_group: None,
            mail: None,
            collect_retention_days: Some(DEFAULT_COLLECT_RETENTION_DAYS),
            session_ttl_hours: Some(DEFAULT_SESSION_TTL_HOURS),
        }
    }

    /// The configured controller-tier runner group, trimmed, or `None` when
    /// unset / blank. `None` ⇒ controller-tier jobs run nowhere (fail-safe).
    pub fn effective_controller_group(&self) -> Option<&str> {
        self.controller_group
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
    }

    /// The effective dead-agent prune window in days: the stored value if
    /// set, else the built-in default, else `0` (= pruning disabled). The
    /// final `unwrap_or(0)` is the absent-everywhere floor, not a
    /// user-facing default — the cleanup task treats `0` as "don't prune".
    ///
    /// Clamped to [`MAX_AGENT_PRUNE_DAYS`] so the cleanup task's
    /// `now - Duration::days(n)` can never overflow `DateTime` (and panic
    /// the task), even if a value larger than the PUT handler allows was
    /// written to the KV out-of-band.
    pub fn effective_agent_prune_days(&self) -> u32 {
        self.agent_prune_days
            .or(Self::defaults().agent_prune_days)
            .unwrap_or(0)
            .min(MAX_AGENT_PRUNE_DAYS)
    }

    /// The effective collect-bundle retention window in days: the stored
    /// value if set, else the built-in default ([`DEFAULT_COLLECT_RETENTION_DAYS`]).
    /// Floored at 1 and clamped to [`MAX_COLLECT_RETENTION_DAYS`] so an
    /// out-of-band KV write can't reach the Object Store `max_age` unsanitised.
    /// The floor matters specifically because NATS treats `max_age: 0` as
    /// **unlimited** retention, not "evict immediately": a stray `0` would
    /// silently make bundles never expire (defeating the auto-expire intent),
    /// so we coerce it to the shortest real window (1 day) instead. The PUT
    /// handler already rejects `0` / over-cap, so this is the belt-and-braces
    /// path for a hand-edited KV value.
    pub fn effective_collect_retention_days(&self) -> u32 {
        self.collect_retention_days
            .or(Self::defaults().collect_retention_days)
            .unwrap_or(DEFAULT_COLLECT_RETENTION_DAYS)
            .clamp(1, MAX_COLLECT_RETENTION_DAYS)
    }

    /// The effective login-token lifetime in hours: the stored value if
    /// set, else the built-in [`DEFAULT_SESSION_TTL_HOURS`]. Floored at 1
    /// and clamped to [`MAX_SESSION_TTL_HOURS`] so a zero/absent/out-of-band
    /// value can never mint an already-expired or overflow-inducing token.
    /// The PUT handler already rejects `0` / over-cap, so this is the
    /// belt-and-braces path for a hand-edited KV value.
    pub fn effective_session_ttl_hours(&self) -> u32 {
        self.session_ttl_hours
            .or(Self::defaults().session_ttl_hours)
            .unwrap_or(DEFAULT_SESSION_TTL_HOURS)
            .clamp(1, MAX_SESSION_TTL_HOURS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unset() {
        assert_eq!(ServerSettings::default().agent_prune_days, None);
    }

    #[test]
    fn unset_resolves_to_disabled() {
        // No stored value + no built-in default ⇒ effective 0 (disabled).
        assert_eq!(ServerSettings::default().effective_agent_prune_days(), 0);
    }

    #[test]
    fn stored_value_wins_over_default() {
        let s = ServerSettings {
            agent_prune_days: Some(30),
            ..Default::default()
        };
        assert_eq!(s.effective_agent_prune_days(), 30);
    }

    #[test]
    fn effective_clamps_to_max() {
        // An out-of-band KV write larger than the PUT cap must not reach
        // the cleanup task unclamped (else its DateTime subtraction panics).
        let s = ServerSettings {
            agent_prune_days: Some(u32::MAX),
            ..Default::default()
        };
        assert_eq!(s.effective_agent_prune_days(), MAX_AGENT_PRUNE_DAYS);
    }

    #[test]
    fn round_trips_through_json() {
        let s = ServerSettings {
            agent_prune_days: Some(30),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"agent_prune_days":30}"#);
        let back: ServerSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn unset_serialises_to_empty_object() {
        // `skip_serializing_if` keeps an all-unset doc minimal; it must
        // round-trip back to all-`None`.
        let s = ServerSettings::default();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "{}");
        let back: ServerSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn explicit_null_decodes_to_unset() {
        let s: ServerSettings = serde_json::from_str(r#"{"agent_prune_days":null}"#).unwrap();
        assert_eq!(s.agent_prune_days, None);
    }

    #[test]
    fn empty_object_decodes_to_default() {
        // A freshly-created key (or one written by an older backend that
        // didn't know this field) must read back as the pre-feature
        // behaviour, not fail to decode.
        let s: ServerSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, ServerSettings::default());
    }

    #[test]
    fn controller_group_effective_trims_and_blank_is_unset() {
        assert_eq!(ServerSettings::default().effective_controller_group(), None);
        let s = ServerSettings {
            controller_group: Some("  feed-runners ".into()),
            ..Default::default()
        };
        assert_eq!(s.effective_controller_group(), Some("feed-runners"));
        // A blank/whitespace value reads as unset (fail-safe: no runner).
        let blank = ServerSettings {
            controller_group: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(blank.effective_controller_group(), None);
    }

    #[test]
    fn controller_group_round_trips_and_omits_when_unset() {
        let s = ServerSettings {
            controller_group: Some("infra".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"controller_group":"infra"}"#);
        assert_eq!(serde_json::from_str::<ServerSettings>(&json).unwrap(), s);
        // Unset controller_group is omitted (skip_serializing_if).
        assert_eq!(
            serde_json::to_string(&ServerSettings::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn mail_round_trips_and_omits_when_unset() {
        use crate::config::{MailEncryption, MailSection};

        // Unset mail is omitted (skip_serializing_if) — a mail-less doc
        // stays minimal and decodes back to `None`.
        assert_eq!(
            serde_json::to_string(&ServerSettings::default()).unwrap(),
            "{}"
        );

        let s = ServerSettings {
            mail: Some(MailSection {
                host: "smtp.example.com".into(),
                port: 587,
                encryption: MailEncryption::Starttls,
                from: "kanade-noreply@example.com".into(),
                username: Some("kanade-noreply".into()),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        // Encryption serialises lowercase; the password is never present.
        assert!(json.contains(r#""encryption":"starttls""#), "json: {json}");
        assert!(!json.contains("password"), "password must never serialise");
        assert_eq!(serde_json::from_str::<ServerSettings>(&json).unwrap(), s);
    }

    #[test]
    fn mail_defaults_to_unset() {
        assert_eq!(ServerSettings::default().mail, None);
        // A doc written before this field existed (no `mail` key) decodes
        // to `None` — email stays a no-op, the pre-feature behaviour.
        let s: ServerSettings = serde_json::from_str(r#"{"agent_prune_days":7}"#).unwrap();
        assert_eq!(s.mail, None);
        assert_eq!(s.agent_prune_days, Some(7));
    }

    #[test]
    fn collect_retention_unset_resolves_to_builtin_default() {
        // Blank (the derived Default) must preserve the historical 30-day
        // window, not disable retention.
        assert_eq!(ServerSettings::default().collect_retention_days, None);
        assert_eq!(
            ServerSettings::default().effective_collect_retention_days(),
            DEFAULT_COLLECT_RETENTION_DAYS,
        );
        // The defaults() document surfaces the real default so the SPA can
        // render it as a placeholder.
        assert_eq!(
            ServerSettings::defaults().collect_retention_days,
            Some(DEFAULT_COLLECT_RETENTION_DAYS),
        );
    }

    #[test]
    fn collect_retention_stored_value_wins() {
        let s = ServerSettings {
            collect_retention_days: Some(90),
            ..Default::default()
        };
        assert_eq!(s.effective_collect_retention_days(), 90);
    }

    #[test]
    fn collect_retention_effective_clamps_out_of_band_writes() {
        // A hand-written KV value past the PUT cap (or 0) must be clamped so
        // the reconciled Object Store max_age stays sane.
        let big = ServerSettings {
            collect_retention_days: Some(u32::MAX),
            ..Default::default()
        };
        assert_eq!(
            big.effective_collect_retention_days(),
            MAX_COLLECT_RETENTION_DAYS,
        );
        let zero = ServerSettings {
            collect_retention_days: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.effective_collect_retention_days(), 1);
    }

    #[test]
    fn collect_retention_round_trips_and_omits_when_unset() {
        let s = ServerSettings {
            collect_retention_days: Some(90),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"collect_retention_days":90}"#);
        assert_eq!(serde_json::from_str::<ServerSettings>(&json).unwrap(), s);
        // Unset is omitted so a blank doc stays minimal.
        assert!(
            !serde_json::to_string(&ServerSettings::default())
                .unwrap()
                .contains("collect_retention_days")
        );
    }

    #[test]
    fn session_ttl_unset_resolves_to_builtin_default() {
        // Blank (the derived Default) must resolve to the 24h default rather
        // than 0 (which would mint already-expired tokens).
        assert_eq!(ServerSettings::default().session_ttl_hours, None);
        assert_eq!(
            ServerSettings::default().effective_session_ttl_hours(),
            DEFAULT_SESSION_TTL_HOURS,
        );
        // The defaults() document surfaces the real default so the SPA can
        // render it as a placeholder.
        assert_eq!(
            ServerSettings::defaults().session_ttl_hours,
            Some(DEFAULT_SESSION_TTL_HOURS),
        );
    }

    #[test]
    fn session_ttl_stored_value_wins() {
        let s = ServerSettings {
            session_ttl_hours: Some(72),
            ..Default::default()
        };
        assert_eq!(s.effective_session_ttl_hours(), 72);
    }

    #[test]
    fn session_ttl_effective_clamps_out_of_band_writes() {
        // An out-of-band 0 must floor to 1 (else `now + 0h` is an
        // instantly-expired token); a value past the cap clamps down so
        // login's date math can't overflow.
        let zero = ServerSettings {
            session_ttl_hours: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.effective_session_ttl_hours(), 1);
        let huge = ServerSettings {
            session_ttl_hours: Some(u32::MAX),
            ..Default::default()
        };
        assert_eq!(huge.effective_session_ttl_hours(), MAX_SESSION_TTL_HOURS);
    }

    #[test]
    fn session_ttl_round_trips_and_omits_when_unset() {
        let s = ServerSettings {
            session_ttl_hours: Some(48),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"session_ttl_hours":48}"#);
        assert_eq!(serde_json::from_str::<ServerSettings>(&json).unwrap(), s);
        // Unset is omitted so a blank doc stays minimal.
        assert!(
            !serde_json::to_string(&ServerSettings::default())
                .unwrap()
                .contains("session_ttl_hours")
        );
    }

    #[test]
    fn accepts_unknown_fields_for_forward_compat() {
        // A newer backend may have added knobs this build doesn't know;
        // decoding must drop them rather than error.
        let json = r#"{"agent_prune_days":7,"some_future_knob":true}"#;
        let s: ServerSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.agent_prune_days, Some(7));
    }
}
