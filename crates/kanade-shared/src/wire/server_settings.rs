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

/// Built-in default for [`ServerSettings::check_status_stale_days`] (#1032②) —
/// how many days a `check_status` row may go without a fresh result before the
/// Compliance view treats it as **stale** and hides it (a PC that stopped
/// running a check: excluded from its target via a dynamic group, decommissioned,
/// or its schedule removed). 30 days is comfortably longer than a typical daily
/// / weekly compliance cadence, so an in-scope PC is never mistakenly hidden,
/// while a genuinely out-of-scope PC's frozen row drops out within the month.
/// A real default (like `collect_retention_days`) so the feature works out of
/// the box; the operator shortens it for prompter hiding or sets `0` to disable.
pub const DEFAULT_CHECK_STATUS_STALE_DAYS: u32 = 30;

/// Upper bound on [`ServerSettings::check_status_stale_days`] (10 years).
/// Staleness is purely a display cutoff (`now - Duration::days(n)` compared to a
/// row's `recorded_at`), so the ceiling just keeps that subtraction inside
/// `chrono::DateTime`'s range and stops an absurd value; enforced by the PUT
/// handler and clamped in [`ServerSettings::effective_check_status_stale_days`].
pub const MAX_CHECK_STATUS_STALE_DAYS: u32 = 3650;

/// Built-in default for [`SupportCode::ttl_minutes`] — how long an unlock
/// grant lasts when the operator hasn't set a per-code window. 15 minutes is
/// about the length of a helpdesk call: long enough that the desk isn't
/// re-typing the code mid-troubleshoot, short enough that a machine left
/// unattended after the call re-locks on its own.
pub const DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES: u32 = 15;

/// Upper bound on [`SupportCode::ttl_minutes`] (8 hours). An unlock grant is
/// standing permission for an end user's machine to run privileged jobs, so
/// it must not outlive a working day even by operator error — a longer window
/// is a `visible_to` targeting decision, not an unlock one. Enforced by the
/// support-code endpoint and clamped in [`SupportCode::effective_ttl_minutes`]
/// so a hand-written KV value stays bounded too.
pub const MAX_SUPPORT_UNLOCK_TTL_MINUTES: u32 = 480;

/// One operator-issued support code — the "裏コマンド" that reveals
/// `client.unlock`-scoped jobs in the Client App (see
/// [`crate::manifest::ClientHint::unlock`]).
///
/// **Only the argon2id hash is stored.** Verification needs no plaintext, so
/// unlike the SMTP password (#884, which must stay in the HKLM registry
/// because SMTP AUTH needs the real string) a support code can live in KV
/// safely — an agent reads the hash to verify a typed code locally, which is
/// what keeps unlocking working while the backend is down, exactly when the
/// desk needs it most.
///
/// The plaintext exists only in transit: the operator types it once into the
/// SPA, the backend hashes it, and no layer ever stores or returns it. The
/// API blanks [`hash`](Self::hash) on the way out too — an operator rotating
/// a code sets a new one, they never read the old one back.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SupportCode {
    /// The scope this code opens, matched byte-for-byte against a job's
    /// `client.unlock`. Unique within the list; a slug (`[A-Za-z0-9._-]`).
    pub scope: String,
    /// argon2id PHC-format hash of the code. Empty ⇒ **no code configured**
    /// for this scope, which fails closed (nothing can be unlocked with it) —
    /// that's also what an API response looks like, since the hash is blanked
    /// before it leaves the backend.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hash: String,
    /// Human label for the code (`"ヘルプデスク一次窓口"`), shown in the
    /// Client App's support-mode banner so the user can see which desk opened
    /// their machine. `None` ⇒ the banner falls back to the scope slug.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// How long a grant from this code lasts, in minutes. `None` ⇒
    /// [`DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES`]. Clamped to
    /// `1..=`[`MAX_SUPPORT_UNLOCK_TTL_MINUTES`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_minutes: Option<u32>,
    /// Temporarily suspend the code without deleting it (and without having
    /// to re-issue a new secret afterwards) — a disabled code never verifies.
    /// `false` (the default) ⇒ live.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

impl SupportCode {
    /// The grant window this code mints, in minutes: the configured value if
    /// set, else [`DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES`]. Floored at 1 and
    /// clamped to [`MAX_SUPPORT_UNLOCK_TTL_MINUTES`] so a `0` (which would
    /// mint an already-expired grant, i.e. an unlock that silently does
    /// nothing) or an out-of-band giant value can't reach the grant store.
    pub fn effective_ttl_minutes(&self) -> u32 {
        self.ttl_minutes
            .unwrap_or(DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES)
            .clamp(1, MAX_SUPPORT_UNLOCK_TTL_MINUTES)
    }

    /// Whether this entry can ever grant anything: live and carrying a hash.
    /// Both halves fail closed — a blanked (API-redacted) or hand-cleared
    /// hash opens nothing, and neither does a disabled code.
    pub fn is_usable(&self) -> bool {
        !self.disabled && !self.hash.is_empty()
    }
}

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

    /// Days a `check_status` row may go without a fresh result before the
    /// Compliance page treats it as **stale** and hides it (#1032②). A PC that
    /// stops running a check — excluded from its target via a dynamic group,
    /// decommissioned, or its schedule removed — stops refreshing its row's
    /// `recorded_at`; once that timestamp is older than this window the row is
    /// omitted from the default `/api/checks` view and its counts, so a machine
    /// no longer in scope stops showing as permanently failing a check it never
    /// runs.
    ///
    /// Like [`collect_retention_days`](Self::collect_retention_days) this carries
    /// a **real built-in default** ([`DEFAULT_CHECK_STATUS_STALE_DAYS`], 30d), so
    /// the feature works with no configuration. `0` **disables** staleness
    /// (every row shown, the pre-feature behaviour); a positive value is the
    /// cutoff, clamped to [`MAX_CHECK_STATUS_STALE_DAYS`]. The row is never
    /// deleted — hiding is non-destructive so history and the compliance-alert
    /// prior-status are preserved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_status_stale_days: Option<u32>,

    /// Operator-issued support codes — the helpdesk "裏コマンド" that reveals
    /// `client.unlock`-scoped jobs in the Client App. See [`SupportCode`].
    ///
    /// The one non-`Option` field in this document, because a list has an
    /// honest empty state: `[]` (the default) means **no codes configured**,
    /// so every `client.unlock` job stays hidden from everyone — the same
    /// fail-closed "behave as before" the `None`s give the other fields.
    ///
    /// Unlike the rest of the document this is read by **agents** as well as
    /// the backend (each agent verifies a typed code against these hashes
    /// locally, so an unlock still works during a backend outage), and it is
    /// **not** editable through the generic `PUT /api/server-settings` merge:
    /// a secret gets its own set-and-forget endpoint so a redacted document
    /// round-tripping through the SPA form can never blank a live code.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub support_codes: Vec<SupportCode>,
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
            check_status_stale_days: Some(DEFAULT_CHECK_STATUS_STALE_DAYS),
            // No built-in code: a deployment that configures none has no
            // unlockable jobs, which is the only safe default for a secret.
            support_codes: Vec::new(),
        }
    }

    /// The live code for `scope`, or `None` when the scope has no code, its
    /// code is disabled, or the hash was blanked (an API-redacted document).
    /// Every caller of this is a gate, so all three cases fail closed.
    pub fn support_code(&self, scope: &str) -> Option<&SupportCode> {
        self.support_codes
            .iter()
            .find(|c| c.scope == scope && c.is_usable())
    }

    /// The same document with every support-code hash blanked — what an HTTP
    /// response is allowed to contain. Scope / label / TTL stay visible so
    /// the SPA can list and manage the codes; the secret material never
    /// leaves the backend, not even to an operator-authenticated caller.
    /// (`GET /api/server-settings` is viewer+, so an unredacted document
    /// would hand every read-only account an offline-crackable hash.)
    #[must_use]
    pub fn redacted(mut self) -> Self {
        for c in &mut self.support_codes {
            c.hash.clear();
        }
        self
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

    /// The effective check-staleness window in days: the stored value if set,
    /// else the built-in [`DEFAULT_CHECK_STATUS_STALE_DAYS`]. **`0` means
    /// disabled** (no row is ever hidden as stale) — deliberately NOT floored to
    /// 1 (unlike collect/session), because 0 is a meaningful "show everything"
    /// value here, the same convention as [`agent_prune_days`](Self::agent_prune_days).
    /// Clamped to [`MAX_CHECK_STATUS_STALE_DAYS`] so an out-of-band KV write
    /// can't overflow the `now - Duration::days(n)` cutoff math.
    pub fn effective_check_status_stale_days(&self) -> u32 {
        self.check_status_stale_days
            .or(Self::defaults().check_status_stale_days)
            .unwrap_or(DEFAULT_CHECK_STATUS_STALE_DAYS)
            .min(MAX_CHECK_STATUS_STALE_DAYS)
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
    fn check_stale_unset_resolves_to_builtin_default() {
        // Blank (the derived Default) resolves to the 30-day default — ON out
        // of the box, so the feature works without configuration.
        assert_eq!(ServerSettings::default().check_status_stale_days, None);
        assert_eq!(
            ServerSettings::default().effective_check_status_stale_days(),
            DEFAULT_CHECK_STATUS_STALE_DAYS,
        );
        // defaults() surfaces the real default for the SPA placeholder.
        assert_eq!(
            ServerSettings::defaults().check_status_stale_days,
            Some(DEFAULT_CHECK_STATUS_STALE_DAYS),
        );
    }

    #[test]
    fn check_stale_zero_disables() {
        // Explicit 0 means "disable staleness" (show everything) — NOT floored
        // to 1 like collect/session; same convention as agent_prune_days.
        let s = ServerSettings {
            check_status_stale_days: Some(0),
            ..Default::default()
        };
        assert_eq!(s.effective_check_status_stale_days(), 0);
    }

    #[test]
    fn check_stale_stored_value_wins_and_clamps() {
        let s = ServerSettings {
            check_status_stale_days: Some(7),
            ..Default::default()
        };
        assert_eq!(s.effective_check_status_stale_days(), 7);
        let big = ServerSettings {
            check_status_stale_days: Some(u32::MAX),
            ..Default::default()
        };
        assert_eq!(
            big.effective_check_status_stale_days(),
            MAX_CHECK_STATUS_STALE_DAYS,
        );
    }

    #[test]
    fn check_stale_round_trips_and_omits_when_unset() {
        let s = ServerSettings {
            check_status_stale_days: Some(14),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"check_status_stale_days":14}"#);
        assert_eq!(serde_json::from_str::<ServerSettings>(&json).unwrap(), s);
        assert!(
            !serde_json::to_string(&ServerSettings::default())
                .unwrap()
                .contains("check_status_stale_days")
        );
    }

    #[test]
    fn support_codes_absent_by_default_and_omitted_from_the_wire() {
        // The pre-feature document must round-trip byte-identically: no
        // `support_codes` key, so an older backend / agent reading it sees
        // exactly what it saw before, and nothing is unlockable.
        let s = ServerSettings::default();
        assert!(s.support_codes.is_empty());
        assert_eq!(serde_json::to_string(&s).unwrap(), "{}");
        assert!(s.support_code("support").is_none());
    }

    #[test]
    fn support_code_lookup_fails_closed() {
        let s = ServerSettings {
            support_codes: vec![
                SupportCode {
                    scope: "support".into(),
                    hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
                    label: Some("ヘルプデスク".into()),
                    ..Default::default()
                },
                SupportCode {
                    scope: "admin".into(),
                    hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
                    disabled: true,
                    ..Default::default()
                },
                SupportCode {
                    // Hash blanked — what an API-redacted document looks like.
                    // It must never be treated as a live code.
                    scope: "blank".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(s.support_code("support").is_some());
        assert!(s.support_code("admin").is_none(), "disabled must not match");
        assert!(
            s.support_code("blank").is_none(),
            "blank hash must not match"
        );
        assert!(s.support_code("nope").is_none());
    }

    #[test]
    fn redacted_blanks_hashes_but_keeps_the_roster() {
        // The SPA still needs to see WHICH scopes exist (to list / rotate /
        // delete them); it must never see the hash.
        let s = ServerSettings {
            support_codes: vec![SupportCode {
                scope: "support".into(),
                hash: "$argon2id$secret".into(),
                label: Some("ヘルプデスク".into()),
                ttl_minutes: Some(30),
                disabled: false,
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&s.redacted()).unwrap();
        assert!(!json.contains("argon2"), "wire leaked the hash: {json}");
        assert!(!json.contains("hash"), "wire leaked the hash key: {json}");
        assert!(json.contains("\"scope\":\"support\""), "wire: {json}");
        assert!(json.contains("\"ttl_minutes\":30"), "wire: {json}");
    }

    #[test]
    fn support_code_ttl_clamps() {
        let unset = SupportCode::default();
        assert_eq!(
            unset.effective_ttl_minutes(),
            DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES
        );
        // 0 would mint an already-expired grant (an unlock that silently
        // does nothing) — floored to the shortest real window instead.
        let zero = SupportCode {
            ttl_minutes: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.effective_ttl_minutes(), 1);
        let huge = SupportCode {
            ttl_minutes: Some(u32::MAX),
            ..Default::default()
        };
        assert_eq!(huge.effective_ttl_minutes(), MAX_SUPPORT_UNLOCK_TTL_MINUTES);
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
