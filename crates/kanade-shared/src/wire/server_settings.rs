use serde::{Deserialize, Serialize};

/// Upper bound on [`ServerSettings::agent_prune_days`] (100 years). A
/// value this large already means "effectively never", and it keeps the
/// cleanup task's `now - Duration::days(n)` subtraction comfortably inside
/// `chrono::DateTime`'s representable range — `DateTime - Duration` panics
/// on overflow, and an unbounded `u32` (~11.7 M years) would trip it.
/// Enforced two ways: the PUT handler rejects a larger value, and
/// [`ServerSettings::effective_agent_prune_days`] clamps to it so even a
/// hand-written KV value can never panic the cleanup task.
pub const MAX_AGENT_PRUNE_DAYS: u32 = 36_500;

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
}

impl ServerSettings {
    /// Built-in defaults applied when a field is unset (`None`) in the
    /// stored document. Currently every field is `None`: there's no
    /// fleet-meaningful default prune window, so leaving it blank means
    /// "disabled" rather than some arbitrary number of days.
    ///
    /// Exposed via `GET /api/server-settings/defaults` so the SPA renders
    /// these as faint placeholders (mirroring the agent layered-config
    /// page's built-in floor). Introducing a real default later is a
    /// one-line change here that automatically shows up in the UI and
    /// applies to every deployment that hasn't overridden the field.
    pub fn defaults() -> Self {
        Self {
            agent_prune_days: None,
            controller_group: None,
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
    fn accepts_unknown_fields_for_forward_compat() {
        // A newer backend may have added knobs this build doesn't know;
        // decoding must drop them rather than error.
        let json = r#"{"agent_prune_days":7,"some_future_knob":true}"#;
        let s: ServerSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.agent_prune_days, Some(7));
    }
}
