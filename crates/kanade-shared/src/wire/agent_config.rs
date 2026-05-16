//! Layered fleet configuration that lives in the `agent_config` KV
//! bucket (Sprint 6).
//!
//! Three scopes flow into the agent's effective config, in order of
//! increasing specificity:
//!
//! ```text
//! built-in default        (compiled in; floor when nothing else is set)
//!   ↓
//! agent_config:global     (whole-fleet default)
//!   ↓
//! agent_config:groups.<g> (per-group override; one or more apply)
//!   ↓
//! agent_config:pcs.<pc>   (per-PC override; final word)
//! ```
//!
//! The wire type for every scope is the same — [`ConfigScope`], a
//! struct of `Option<T>` fields. `Some` means "this scope sets this
//! field"; `None` means "fall through to the next layer". JSON
//! `null` is the same as the field being absent thanks to serde's
//! struct-level `default`.
//!
//! [`resolve`] is the pure functional core that flattens the scope
//! stack into an [`EffectiveConfig`] (concrete values, no Options).
//! When the same field is set on more than one group the PC belongs
//! to, alphabetical group order wins last (CSS-cascade style) and a
//! [`ResolutionWarning::MultiGroupConflict`] is emitted so the
//! caller can log it — pre-empts the "why does this PC have value X?
//! none of my groups say X" debugging session.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-scope partial config. Every field is `Option<T>`: `Some` =
/// set, `None` = inherit from the next-less-specific scope. Serde
/// `default` + `skip_serializing_if` keeps the wire JSON tight —
/// unset fields don't appear in the bucket value.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ConfigScope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_interval: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_jitter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval: Option<String>,
}

impl ConfigScope {
    pub fn is_empty(&self) -> bool {
        self.target_version.is_none()
            && self.inventory_interval.is_none()
            && self.inventory_jitter.is_none()
            && self.inventory_enabled.is_none()
            && self.heartbeat_interval.is_none()
    }
}

/// Concrete config the agent runs against once the scope stack has
/// been flattened. `target_version` stays `Option` because "no
/// rollout target set anywhere" is a meaningful state (the agent
/// just keeps running the version it has); the other fields always
/// have a value, falling back to [`EffectiveConfig::builtin_defaults`]
/// when no scope sets them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub target_version: Option<String>,
    pub inventory_interval: String,
    pub inventory_jitter: String,
    pub inventory_enabled: bool,
    pub heartbeat_interval: String,
}

impl EffectiveConfig {
    /// Floor values used when no KV scope sets a given field.
    /// Mirrors the historic agent.toml defaults so unbootstrapped
    /// fleets keep behaving the way they did pre-Sprint 6.
    pub fn builtin_defaults() -> Self {
        Self {
            target_version: None,
            inventory_interval: "24h".to_string(),
            inventory_jitter: "10m".to_string(),
            inventory_enabled: true,
            heartbeat_interval: "30s".to_string(),
        }
    }
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self::builtin_defaults()
    }
}

/// Non-fatal observations from [`resolve`] that the caller should
/// log. Currently only "two of this PC's groups set the same field
/// to different values" — useful pre-emptive debugging signal when
/// canary / wave / dept overlays accidentally overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionWarning {
    MultiGroupConflict {
        field: &'static str,
        /// Group names that set this field, in alphabetical order
        /// (i.e. the application order — the last name in this list
        /// is the one whose value actually won).
        groups: Vec<String>,
    },
}

/// Flatten the scope stack into an [`EffectiveConfig`].
///
/// * `global` — the `global` key in the `agent_config` bucket
///   (`None` if no row yet).
/// * `group_scopes` — every `groups.<name>` row currently in the
///   bucket (the caller can pass all of them; only the ones whose
///   name is in `my_groups` are applied).
/// * `pc_scope` — the `pcs.<pc_id>` row for this agent (`None` if
///   no row yet).
/// * `my_groups` — this agent's current memberships (from the
///   `agent_groups` bucket).
///
/// Order of application: built-in default → global → per-group
/// (alphabetical, last wins) → per-pc. Multi-group conflicts (≥ 2
/// of `my_groups` setting the same field) are returned as warnings
/// alongside the resolved config.
pub fn resolve(
    global: Option<&ConfigScope>,
    group_scopes: &BTreeMap<String, ConfigScope>,
    pc_scope: Option<&ConfigScope>,
    my_groups: &[String],
) -> (EffectiveConfig, Vec<ResolutionWarning>) {
    let mut out = EffectiveConfig::builtin_defaults();
    let mut warnings = Vec::new();

    if let Some(g) = global {
        apply_scope(&mut out, g);
    }

    // Sort + dedup the group list so iteration order is deterministic
    // and "last wins" is well-defined.
    let mut sorted_groups: Vec<&str> = my_groups.iter().map(String::as_str).collect();
    sorted_groups.sort();
    sorted_groups.dedup();

    // Pass 1: find multi-setter fields so the caller can warn before
    // pass 2 silently lets the alphabetical-last value win.
    let mut setters: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for g in &sorted_groups {
        let Some(scope) = group_scopes.get(*g) else {
            continue;
        };
        if scope.target_version.is_some() {
            setters.entry("target_version").or_default().push(g.to_string());
        }
        if scope.inventory_interval.is_some() {
            setters.entry("inventory_interval").or_default().push(g.to_string());
        }
        if scope.inventory_jitter.is_some() {
            setters.entry("inventory_jitter").or_default().push(g.to_string());
        }
        if scope.inventory_enabled.is_some() {
            setters.entry("inventory_enabled").or_default().push(g.to_string());
        }
        if scope.heartbeat_interval.is_some() {
            setters.entry("heartbeat_interval").or_default().push(g.to_string());
        }
    }
    for (field, groups) in setters {
        if groups.len() > 1 {
            warnings.push(ResolutionWarning::MultiGroupConflict {
                field,
                groups,
            });
        }
    }

    // Pass 2: actually apply, alphabetically. Last-wins by construction.
    for g in &sorted_groups {
        if let Some(scope) = group_scopes.get(*g) {
            apply_scope(&mut out, scope);
        }
    }

    if let Some(p) = pc_scope {
        apply_scope(&mut out, p);
    }

    (out, warnings)
}

fn apply_scope(out: &mut EffectiveConfig, s: &ConfigScope) {
    if let Some(v) = &s.target_version {
        out.target_version = Some(v.clone());
    }
    if let Some(v) = &s.inventory_interval {
        out.inventory_interval = v.clone();
    }
    if let Some(v) = &s.inventory_jitter {
        out.inventory_jitter = v.clone();
    }
    if let Some(v) = s.inventory_enabled {
        out.inventory_enabled = v;
    }
    if let Some(v) = &s.heartbeat_interval {
        out.heartbeat_interval = v.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ConfigScope {
        ConfigScope::default()
    }

    #[test]
    fn empty_stack_gives_builtin_defaults() {
        let (eff, warns) = resolve(None, &BTreeMap::new(), None, &[]);
        assert_eq!(eff, EffectiveConfig::builtin_defaults());
        assert!(warns.is_empty());
    }

    #[test]
    fn global_only() {
        let g = ConfigScope {
            inventory_interval: Some("12h".into()),
            heartbeat_interval: Some("60s".into()),
            ..scope()
        };
        let (eff, _) = resolve(Some(&g), &BTreeMap::new(), None, &[]);
        assert_eq!(eff.inventory_interval, "12h");
        assert_eq!(eff.heartbeat_interval, "60s");
        // Unset fields stay at builtin defaults.
        assert_eq!(eff.inventory_jitter, "10m");
        assert!(eff.inventory_enabled);
        assert!(eff.target_version.is_none());
    }

    #[test]
    fn group_overrides_global() {
        let global = ConfigScope {
            inventory_interval: Some("24h".into()),
            ..scope()
        };
        let mut groups = BTreeMap::new();
        groups.insert(
            "canary".into(),
            ConfigScope {
                inventory_interval: Some("1h".into()),
                ..scope()
            },
        );
        let (eff, warns) = resolve(Some(&global), &groups, None, &["canary".into()]);
        assert_eq!(eff.inventory_interval, "1h");
        assert!(warns.is_empty());
    }

    #[test]
    fn pc_overrides_group() {
        let mut groups = BTreeMap::new();
        groups.insert(
            "wave1".into(),
            ConfigScope {
                inventory_interval: Some("12h".into()),
                ..scope()
            },
        );
        let pc = ConfigScope {
            inventory_interval: Some("5m".into()),
            ..scope()
        };
        let (eff, _) = resolve(None, &groups, Some(&pc), &["wave1".into()]);
        assert_eq!(eff.inventory_interval, "5m");
    }

    #[test]
    fn pc_overrides_global_when_no_group_match() {
        let global = ConfigScope {
            inventory_interval: Some("24h".into()),
            ..scope()
        };
        let pc = ConfigScope {
            inventory_interval: Some("30m".into()),
            ..scope()
        };
        let (eff, _) = resolve(Some(&global), &BTreeMap::new(), Some(&pc), &[]);
        assert_eq!(eff.inventory_interval, "30m");
    }

    #[test]
    fn partial_override_only_changes_named_fields() {
        let global = ConfigScope {
            inventory_interval: Some("24h".into()),
            heartbeat_interval: Some("30s".into()),
            ..scope()
        };
        let pc = ConfigScope {
            heartbeat_interval: Some("15s".into()),
            // intentionally not touching inventory_interval
            ..scope()
        };
        let (eff, _) = resolve(Some(&global), &BTreeMap::new(), Some(&pc), &[]);
        assert_eq!(eff.inventory_interval, "24h"); // from global
        assert_eq!(eff.heartbeat_interval, "15s"); // from pc
    }

    #[test]
    fn multi_group_conflict_emits_warning() {
        let mut groups = BTreeMap::new();
        groups.insert(
            "wave1".into(),
            ConfigScope {
                inventory_interval: Some("12h".into()),
                ..scope()
            },
        );
        groups.insert(
            "dept-eng".into(),
            ConfigScope {
                inventory_interval: Some("24h".into()),
                ..scope()
            },
        );
        let (eff, warns) = resolve(
            None,
            &groups,
            None,
            &["wave1".into(), "dept-eng".into()],
        );
        // "dept-eng" sorts before "wave1", so wave1 wins (last alphabetical).
        assert_eq!(eff.inventory_interval, "12h");
        assert_eq!(warns.len(), 1);
        match &warns[0] {
            ResolutionWarning::MultiGroupConflict { field, groups } => {
                assert_eq!(*field, "inventory_interval");
                assert_eq!(groups, &vec!["dept-eng".to_string(), "wave1".to_string()]);
            }
        }
    }

    #[test]
    fn group_alphabetical_last_wins_no_conflict_when_only_one_sets() {
        let mut groups = BTreeMap::new();
        groups.insert(
            "wave1".into(),
            ConfigScope {
                inventory_interval: Some("12h".into()),
                ..scope()
            },
        );
        groups.insert(
            "dept-eng".into(),
            ConfigScope {
                // Different field — doesn't conflict.
                heartbeat_interval: Some("15s".into()),
                ..scope()
            },
        );
        let (eff, warns) = resolve(
            None,
            &groups,
            None,
            &["wave1".into(), "dept-eng".into()],
        );
        assert_eq!(eff.inventory_interval, "12h");
        assert_eq!(eff.heartbeat_interval, "15s");
        assert!(warns.is_empty());
    }

    #[test]
    fn unknown_group_is_silently_ignored() {
        // my_groups names a group that has no scope row yet. Common
        // on the first agent that joins a freshly-named group; the
        // resolver should treat it as a no-op, not an error.
        let mut groups = BTreeMap::new();
        groups.insert(
            "canary".into(),
            ConfigScope {
                inventory_interval: Some("1h".into()),
                ..scope()
            },
        );
        let (eff, warns) = resolve(
            None,
            &groups,
            None,
            &["canary".into(), "ghost-group".into()],
        );
        assert_eq!(eff.inventory_interval, "1h");
        assert!(warns.is_empty());
    }

    #[test]
    fn group_scope_not_applied_when_pc_not_in_group() {
        let mut groups = BTreeMap::new();
        groups.insert(
            "canary".into(),
            ConfigScope {
                target_version: Some("0.3.0".into()),
                ..scope()
            },
        );
        let (eff, _) = resolve(None, &groups, None, &["dept-eng".into()]);
        // PC is NOT in canary, so the rollout target shouldn't apply.
        assert!(eff.target_version.is_none());
    }

    #[test]
    fn duplicate_group_names_dedup_silently() {
        let mut groups = BTreeMap::new();
        groups.insert(
            "wave1".into(),
            ConfigScope {
                inventory_interval: Some("12h".into()),
                ..scope()
            },
        );
        // my_groups carries the same name twice — the dedup pass
        // keeps it from looking like a conflict-with-self.
        let (eff, warns) = resolve(
            None,
            &groups,
            None,
            &["wave1".into(), "wave1".into()],
        );
        assert_eq!(eff.inventory_interval, "12h");
        assert!(warns.is_empty());
    }

    #[test]
    fn config_scope_serde_round_trip() {
        let s = ConfigScope {
            target_version: Some("0.3.0".into()),
            heartbeat_interval: Some("15s".into()),
            ..scope()
        };
        let json = serde_json::to_string(&s).unwrap();
        // Only set fields appear in JSON.
        assert_eq!(
            json,
            r#"{"target_version":"0.3.0","heartbeat_interval":"15s"}"#
        );
        let back: ConfigScope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn empty_config_scope_round_trips_as_empty_json() {
        let s = ConfigScope::default();
        assert!(s.is_empty());
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "{}");
        let back: ConfigScope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn deserialize_tolerates_unknown_fields_for_forward_compat() {
        // Sprint 6+ may add fields (log_level, jitter strategy, …);
        // older agent / backend builds should keep parsing.
        let json = r#"{"target_version":"0.3.0","future_knob":"future_value"}"#;
        let s: ConfigScope = serde_json::from_str(json).unwrap();
        assert_eq!(s.target_version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn pc_does_not_override_other_pcs() {
        // Sanity: pc_scope passed in is by definition the row for THIS
        // pc; the caller is responsible for picking the right one.
        // This test guards against a future refactor that accidentally
        // wires in the wrong scope by ensuring the apply happens last
        // (after groups), so the PC value is the visible one.
        let mut groups = BTreeMap::new();
        groups.insert(
            "wave1".into(),
            ConfigScope {
                inventory_interval: Some("12h".into()),
                ..scope()
            },
        );
        let pc = ConfigScope {
            inventory_interval: Some("5m".into()),
            ..scope()
        };
        let (eff, _) = resolve(None, &groups, Some(&pc), &["wave1".into()]);
        assert_eq!(eff.inventory_interval, "5m");
    }
}
