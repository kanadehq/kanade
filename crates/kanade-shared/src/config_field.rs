//! Field/value grammar for `agent_config` scope edits.
//!
//! The CLI validates up front and the backend validates again at its
//! write boundary; both call the functions here so the two can never
//! disagree about which `<field>=<value>` specs are acceptable or what
//! the error says.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::wire::ConfigScope;

/// Request body of the backend's single-field set route
/// (`PUT …/config/fields/{field}`). Unset has no body — it is the
/// `DELETE` of the same route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FieldValue {
    pub value: String,
}

/// Response of the backend's single-field set / unset routes: the scope
/// as it stands afterwards, and whether the call actually wrote. An
/// already-satisfied request answers `changed: false` without touching
/// the KV row, so the revision does not move and no watcher wakes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FieldUpdate {
    pub scope: ConfigScope,
    pub changed: bool,
}

/// Split a `<field>=<value>` spec (the argument of `kanade config set`).
/// Only the first `=` separates, so values may contain `=`.
pub fn parse_set_spec(spec: &str) -> Result<(&str, &str)> {
    spec.split_once('=')
        .ok_or_else(|| anyhow::anyhow!("expected <field>=<value>, got '{spec}'"))
}

/// Apply `value` (or `None` for unset) to the named field on
/// `scope`. An open-coded match rather than a generic helper because
/// the field names + types are stable enough that it is the most
/// readable form.
pub fn apply_field(scope: &mut ConfigScope, field: &str, value: Option<&str>) -> Result<()> {
    // #491: duration fields are humantime-validated BEFORE the KV
    // put. The agent maps an unparseable value to a silent fallback
    // (jitter especially used to fall back to ZERO — turning a
    // `30minutes`-style typo into a fleet-wide simultaneous-download
    // herd on the next rollout), so the only safe place to catch the
    // typo is the write boundary, where the operator gets an error.
    let parsed_duration = |field: &str, v: Option<&str>| -> Result<Option<String>> {
        match v {
            None => Ok(None),
            Some(v) => {
                humantime::parse_duration(v).with_context(|| {
                    format!("{field}: expected a humantime duration (e.g. 30s, 10m, 1h), got {v:?}")
                })?;
                Ok(Some(v.to_string()))
            }
        }
    };
    match field {
        "max_local_concurrent" => {
            scope.max_local_concurrent = value
                .map(str::parse::<std::num::NonZeroU32>)
                .transpose()
                .context("max_local_concurrent: expected an integer >= 1")?;
        }
        "target_version" => scope.target_version = value.map(String::from),
        "target_version_jitter" => {
            scope.target_version_jitter = parsed_duration(field, value)?;
        }
        "heartbeat_interval" => scope.heartbeat_interval = parsed_duration(field, value)?,
        "host_perf_interval" => scope.host_perf_interval = parsed_duration(field, value)?,
        "process_perf_enabled" => {
            scope.process_perf_enabled = match value {
                None => None,
                Some(v) => Some(v.parse::<bool>().with_context(|| {
                    format!("process_perf_enabled: expected true|false, got {v:?}")
                })?),
            };
        }
        "process_perf_expires_at" => {
            scope.process_perf_expires_at = match value {
                None => None,
                Some(v) => Some(
                    chrono::DateTime::parse_from_rfc3339(v)
                        .with_context(|| {
                            format!(
                                "process_perf_expires_at: expected RFC3339 timestamp, got {v:?}"
                            )
                        })?
                        .with_timezone(&chrono::Utc),
                ),
            };
        }
        "process_perf_top_n" => {
            scope.process_perf_top_n = match value {
                None => None,
                Some(v) => Some(v.parse::<u32>().with_context(|| {
                    format!("process_perf_top_n: expected positive integer, got {v:?}")
                })?),
            };
        }
        // Free-form product name (e.g. "端末管理支援ツール") — no
        // format validation; any non-empty string is a valid brand.
        // The agent/client trim + treat blank as "unset" downstream.
        "client_display_name" => scope.client_display_name = value.map(String::from),
        other => bail!(
            "unknown field '{other}' — supported: max_local_concurrent, target_version, target_version_jitter, heartbeat_interval, host_perf_interval, process_perf_enabled, process_perf_expires_at, process_perf_top_n, client_display_name"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_limit_sets_clears_and_rejects_invalid_values() {
        let mut s = ConfigScope::default();
        apply_field(&mut s, "max_local_concurrent", Some("2")).unwrap();
        assert_eq!(s.max_local_concurrent.unwrap().get(), 2);
        for value in ["0", "-1", "1.5", "4294967296"] {
            assert!(apply_field(&mut s, "max_local_concurrent", Some(value)).is_err());
        }
        apply_field(&mut s, "max_local_concurrent", None).unwrap();
        assert!(s.max_local_concurrent.is_none());
    }
    #[test]
    fn apply_field_sets_string() {
        let mut s = ConfigScope::default();
        apply_field(&mut s, "heartbeat_interval", Some("15s")).unwrap();
        assert_eq!(s.heartbeat_interval.as_deref(), Some("15s"));
    }

    #[test]
    fn apply_field_unset_clears_string() {
        let mut s = ConfigScope {
            heartbeat_interval: Some("15s".into()),
            ..Default::default()
        };
        apply_field(&mut s, "heartbeat_interval", None).unwrap();
        assert!(s.heartbeat_interval.is_none());
    }

    #[test]
    fn apply_field_sets_and_clears_client_display_name() {
        let mut s = ConfigScope::default();
        apply_field(&mut s, "client_display_name", Some("端末管理支援ツール")).unwrap();
        assert_eq!(s.client_display_name.as_deref(), Some("端末管理支援ツール"));
        apply_field(&mut s, "client_display_name", None).unwrap();
        assert!(s.client_display_name.is_none());
    }

    #[test]
    fn apply_field_rejects_unknown() {
        let mut s = ConfigScope::default();
        let err = apply_field(&mut s, "nope", Some("x")).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn apply_field_rejects_malformed_durations() {
        // #491: a typo'd duration must be rejected at the write
        // boundary, never stored (the agent's parse failure falls
        // back silently — jitter especially used to fall back to
        // ZERO, defeating the rollout stagger fleet-wide).
        let mut s = ConfigScope::default();
        for field in [
            "target_version_jitter",
            "heartbeat_interval",
            "host_perf_interval",
        ] {
            let err = apply_field(&mut s, field, Some("not-a-duration")).unwrap_err();
            assert!(err.to_string().contains("humantime"), "{field}: {err:#}",);
        }
        // Unset still works for validated fields.
        apply_field(&mut s, "target_version_jitter", None).unwrap();
        assert!(s.target_version_jitter.is_none());
    }

    #[test]
    fn parse_set_spec_splits_on_first_equals() {
        assert_eq!(
            parse_set_spec("heartbeat_interval=15s").unwrap(),
            ("heartbeat_interval", "15s")
        );
        assert_eq!(parse_set_spec("a=b=c").unwrap(), ("a", "b=c"));
        assert_eq!(
            parse_set_spec("client_display_name=").unwrap(),
            ("client_display_name", "")
        );
    }

    #[test]
    fn parse_set_spec_rejects_missing_equals() {
        let err = parse_set_spec("heartbeat_interval").unwrap_err();
        assert_eq!(
            err.to_string(),
            "expected <field>=<value>, got 'heartbeat_interval'"
        );
    }
}
