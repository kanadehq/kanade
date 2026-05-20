//! v0.26 — Manifest-declared staleness policy for the Layer 2
//! defense (see SPEC.md §2.6.2).
//!
//! Carries the operator's intent re: "what should the agent do when
//! it can't talk to the broker and so can't verify `script_current`
//! / `script_status`?" — three answers cover the realistic span:
//!
//! - [`Staleness::Cached`] (default) — fire from cache; matches
//!   pre-v0.26 behaviour so existing manifests keep working.
//! - [`Staleness::Strict`] — must have been connected to the broker
//!   within `max_cache_age` of fire time; else skip.
//! - [`Staleness::Unchecked`] — skip the Layer 2 KV checks entirely.
//!
//! Lives in `wire/` because both the YAML-defined `Manifest` and the
//! over-the-wire `Command` carry it (publisher copies it forward so
//! the agent doesn't have to re-look-up the manifest at fire time).

use serde::{Deserialize, Serialize};

/// Staleness policy declared on a Manifest, forwarded onto each
/// emitted Command. Internally tagged via `mode` so YAML reads
/// naturally:
///
/// ```yaml
/// staleness:
///   mode: strict
///   max_cache_age: 5m
/// ```
///
/// `Cached` and `Unchecked` carry no payload, so they collapse to
/// just `staleness: { mode: cached }` / `staleness: { mode: unchecked }`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Staleness {
    /// Use whatever the agent has cached for `script_current` /
    /// `script_status`, no age limit. Silently proceed if the entries
    /// are missing. Historical default — every pre-v0.26 Manifest
    /// reads as this on deserialize, so introducing the field is
    /// fully back-compatible.
    #[default]
    Cached,
    /// Only run when the agent can verify reasonably-fresh KV state.
    /// "Reasonably fresh" = the last confirmed connection to the
    /// broker happened within `max_cache_age` (NATS KV watch is
    /// push-based, so while connected the cache is provably up to
    /// date; the timer only starts when the agent disconnects). When
    /// the window expires the agent attempts a live `kv.get()`; if
    /// *that* also fails the agent publishes a synthetic skipped
    /// result with `exit_code = 127`.
    Strict {
        /// Humantime duration (e.g. `"0s"`, `"5m"`, `"1h"`). Required
        /// for `strict` mode — there's no implicit default because
        /// the right answer is workload-specific.
        ///
        /// `0s` is the conservative choice — agent must be online
        /// right now. Larger values trade urgency for tolerance to
        /// brief network blips.
        max_cache_age: String,
    },
    /// Skip Layer 2 entirely — don't look at `script_current` or
    /// `script_status`. Use only for fully idempotent local scripts
    /// where revoke semantics don't make sense.
    Unchecked,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_round_trips() {
        let s: Staleness = serde_json::from_str(r#"{"mode":"cached"}"#).unwrap();
        assert_eq!(s, Staleness::Cached);
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back["mode"], "cached");
    }

    #[test]
    fn strict_requires_max_cache_age() {
        let s: Staleness =
            serde_json::from_str(r#"{"mode":"strict","max_cache_age":"5m"}"#).unwrap();
        match s {
            Staleness::Strict { max_cache_age } => assert_eq!(max_cache_age, "5m"),
            other => panic!("expected strict, got {other:?}"),
        }
    }

    #[test]
    fn strict_without_max_cache_age_errors() {
        // Operator forgot to set the field — fail loud at parse, not
        // at fire time.
        let res: Result<Staleness, _> = serde_json::from_str(r#"{"mode":"strict"}"#);
        assert!(res.is_err(), "expected error, got {res:?}");
    }

    #[test]
    fn unchecked_round_trips() {
        let s: Staleness = serde_json::from_str(r#"{"mode":"unchecked"}"#).unwrap();
        assert_eq!(s, Staleness::Unchecked);
    }

    #[test]
    fn default_is_cached() {
        // Critical for back-compat — Manifest #[serde(default)] on the
        // staleness field must give us Cached, not e.g. Unchecked.
        assert_eq!(Staleness::default(), Staleness::Cached);
    }

    #[test]
    fn yaml_strict_parses() {
        let yaml = r#"
mode: strict
max_cache_age: 0s
"#;
        let s: Staleness = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(s, Staleness::Strict { .. }));
    }

    #[test]
    fn yaml_cached_minimal_form() {
        let yaml = "mode: cached\n";
        let s: Staleness = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(s, Staleness::Cached);
    }
}
