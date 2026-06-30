//! `state.*` method types — endpoint health snapshot + push notifications.
//!
//! Drives the Client App's "Health" tab (SPEC §2.1 use case 2):
//! BitLocker / AV signature / OS-patch / cert-expiry / disk-free /
//! agent-self-update + arbitrary additional compliance checks. The
//! snapshot is computed agent-side on demand (`state.snapshot`) and
//! pushed when underlying checks flip via `state.changed`.

use serde::{Deserialize, Serialize};

// ---------- state.snapshot ----------

/// `state.snapshot` takes no params.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct StateSnapshotParams {}

/// Full state bundle — the SPA renders this verbatim on the Health
/// tab. SPEC §2.12.8's complete-conversation example pins the
/// shape:
///
/// There is deliberately no dedicated `vpn` field: VPN posture is
/// probe-able from the box and site-specific, so it belongs in
/// [`checks`](StateSnapshot::checks) as an operator-defined `check:`
/// job (same path as `disk_free` / `bitlocker`), not as a hard-coded
/// snapshot field. A site that wants it ships a `check-vpn.yaml`.
///
/// ```jsonc
/// {"pc_id":"PC1234","online":true,
///  "checks":[{"name":"bitlocker","status":"ok"}],
///  "agent_version":"0.4.0","target_version":"0.4.0"}
/// ```
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct StateSnapshot {
    /// Agent's `pc_id` — duplicated here from the handshake so the
    /// SPA can refresh the snapshot independently without
    /// re-handshaking.
    pub pc_id: String,
    /// `true` when the agent currently has a NATS connection open.
    /// Distinct from the OS-level network state — operators care
    /// about "is fleet management reachable" specifically.
    pub online: bool,
    /// Ordered list of compliance check results. Each [`Check`]
    /// item is rendered as a row on the Health tab; failing rows
    /// surface a "修復する" button per SPEC §2.1.
    pub checks: Vec<Check>,
    /// Currently-running agent binary version
    /// (`CARGO_PKG_VERSION`). Same value as
    /// [`super::system::VersionResult::agent_version`].
    pub agent_version: String,
    /// Version the agent self-updater is targeting. When this
    /// differs from `agent_version`, the SPA shows "restart pending"
    /// on the Health tab.
    pub target_version: String,
}

/// One compliance check result. `name` is the stable id (used as
/// React key + analytics label); `label` is the optional human-facing
/// title shown instead of the slug; `status` drives the row's color;
/// `detail` is human-readable text for the row body. `troubleshoot`
/// is the optional `Manifest.id` of the job whose execute button
/// fixes this check — `None` means the check has no auto-remediation.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct Check {
    pub name: String,
    /// Optional human-facing row title. When set, the Client App's
    /// Health tab renders this instead of [`name`](Check::name) — a
    /// `defender_rtp` slug becomes e.g. "ウイルス対策のリアルタイム保護".
    /// Sourced from the check job's
    /// [`CheckHint.label`](crate::manifest::CheckHint::label); `None`
    /// falls back to the slug, so it's purely additive on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub status: CheckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Manifest id of a `category: troubleshoot` job that fixes
    /// this check. The Client App renders a "修復する" button when
    /// present (SPEC §2.1). The job MUST have `user_invokable:
    /// true` — if not, `jobs.execute` returns `Unauthorized` when
    /// the button is clicked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub troubleshoot: Option<String>,
    /// When `true`, the Client App must NOT render this check on its
    /// Health tab (nor count it in the health summary). Set by the agent
    /// from a `check:` job's [`CheckHint.health`](crate::manifest::CheckHint::health)
    /// `== false` — a **gate-only** check that exists purely to drive a
    /// `client.show_when` display gate. The check is STILL carried in the
    /// snapshot (so `show_when` evaluation, which reads the same
    /// `StateSnapshot.checks`, keeps working); only the end-user Health
    /// *rendering* skips it. New field ⇒ #492 wire rule: `default` (absent
    /// ⇒ `false` ⇒ shown, unchanged for old readers) + `skip_serializing_if`
    /// so the overwhelmingly-common shown case stays off the wire.
    #[serde(default, skip_serializing_if = "is_false")]
    pub health_hidden: bool,
}

/// `skip_serializing_if` predicate for a `bool` that defaults to `false`:
/// keep the field off the wire in the common (`false`) case. Clearer than
/// the equivalent `std::ops::Not::not` (which only type-checks via the
/// blanket `impl Not for &bool`).
fn is_false(b: &bool) -> bool {
    !*b
}

/// Four-state result mirroring the SPA's color palette: ok = green,
/// warn = yellow, fail = red, unknown = grey. Wire-encoded as
/// snake_case (`"ok"` / `"warn"` / `"fail"` / `"unknown"`) — the
/// PascalCase convention is reserved for [`super::error::ErrorKind`]
/// where SPEC §2.12.9 specifically pins it.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Check passed.
    Ok,
    /// Non-blocking finding. SPA renders yellow; user can ignore.
    Warn,
    /// Failed — SPA renders red. If a `troubleshoot` manifest is
    /// declared, the "修復する" button is enabled.
    Fail,
    /// Check couldn't run (agent timed out, WMI hang, …). SPA
    /// renders grey "Unknown" — operator should investigate via
    /// `system.log_tail`.
    Unknown,
}

// ---------- state.subscribe ----------

/// `state.subscribe` takes no params.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone, Default)]
pub struct StateSubscribeParams {}

/// `state.subscribe` returns an opaque subscription handle. The
/// client passes it back to `state.unsubscribe` to stop the push
/// stream; SPEC §2.12.7 says subscriptions are auto-cleaned on
/// disconnect, so a well-behaved client never needs to remember
/// these across reconnects.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct StateSubscribeResult {
    pub subscription: String,
}

/// `state.unsubscribe` params.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct StateUnsubscribeParams {
    pub subscription: String,
}

// ---------- state.changed (push) ----------

/// Push payload for `state.changed`. Pushed by the agent when one
/// or more compliance checks flip status, or when `online` /
/// `agent_version` change. A full [`StateSnapshot`] is included
/// so the client doesn't need a second round-trip — the push is
/// strictly idempotent: applying a `state.changed` payload onto the
/// client's cached snapshot is a no-op replace, not a diff merge.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Debug, Clone)]
pub struct StateChangedParams {
    /// Full snapshot at the time of the change.
    pub snapshot: StateSnapshot,
    /// Wall-clock when the agent detected the change. Lets the
    /// client surface "updated 3 s ago" without trusting its own
    /// clock for the agent's processing time.
    pub at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_status_serialises_snake_case() {
        for (variant, expected) in [
            (CheckStatus::Ok, "\"ok\""),
            (CheckStatus::Warn, "\"warn\""),
            (CheckStatus::Fail, "\"fail\""),
            (CheckStatus::Unknown, "\"unknown\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "encode {variant:?}");
            let back: CheckStatus = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant, "round-trip {expected}");
        }
    }

    #[test]
    fn state_snapshot_spec_example_decodes() {
        // SPEC §2.12.8 — pinned so a rename can't drift the
        // documented contract.
        let wire = r#"{
            "pc_id":"PC1234","online":true,
            "checks":[{"name":"bitlocker","status":"ok"},
                      {"name":"av_signature","status":"warn","detail":"3 日前"}],
            "agent_version":"0.4.0","target_version":"0.4.0"
        }"#;
        let s: StateSnapshot = serde_json::from_str(wire).expect("decode");
        assert_eq!(s.pc_id, "PC1234");
        assert!(s.online);
        assert_eq!(s.checks.len(), 2);
        assert_eq!(s.checks[0].name, "bitlocker");
        assert_eq!(s.checks[0].status, CheckStatus::Ok);
        assert_eq!(s.checks[1].name, "av_signature");
        assert_eq!(s.checks[1].status, CheckStatus::Warn);
        assert_eq!(s.checks[1].detail.as_deref(), Some("3 日前"));
        assert_eq!(s.agent_version, "0.4.0");
        assert_eq!(s.target_version, "0.4.0");
    }

    #[test]
    fn check_with_troubleshoot_round_trips() {
        let c = Check {
            name: "av_signature".into(),
            label: Some("ウイルス対策の定義ファイル".into()),
            status: CheckStatus::Fail,
            detail: Some("Signatures > 7 days old".into()),
            troubleshoot: Some("update-av-signatures".into()),
            health_hidden: false,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Check = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, c.name);
        assert_eq!(back.label, c.label);
        assert_eq!(back.status, c.status);
        assert_eq!(back.detail, c.detail);
        assert_eq!(back.troubleshoot, c.troubleshoot);
    }

    #[test]
    fn check_without_optional_fields_decodes() {
        // Minimal check — `label` + `detail` + `troubleshoot` should
        // all be absent on the wire (not `null`) thanks to
        // `skip_serializing_if`.
        let c = Check {
            name: "bitlocker".into(),
            label: None,
            status: CheckStatus::Ok,
            detail: None,
            troubleshoot: None,
            health_hidden: false,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("label").is_none(), "wire: {v:?}");
        assert!(v.get("detail").is_none(), "wire: {v:?}");
        assert!(v.get("troubleshoot").is_none(), "wire: {v:?}");
        // health_hidden = false is the common case → absent on the wire.
        assert!(v.get("health_hidden").is_none(), "wire: {v:?}");
    }

    #[test]
    fn check_status_parses_from_a_free_form_object_field() {
        // The unified model: a check's stdout is an inventory-style
        // object; the agent reads the `status_field` value (default
        // "status") and parses it into a CheckStatus. Pin that the
        // wire encoding the operator writes round-trips.
        let obj: serde_json::Value = serde_json::from_str(
            r#"{"status":"warn","detail":"D: unprotected","volumes":[{"drive":"C:","on":true}]}"#,
        )
        .expect("decode");
        let status: CheckStatus =
            serde_json::from_value(obj.get("status").unwrap().clone()).expect("status parses");
        assert_eq!(status, CheckStatus::Warn);
        assert_eq!(obj.get("detail").unwrap().as_str(), Some("D: unprotected"));
        // The rest of the object is free-form (inventory projects it).
        assert!(obj.get("volumes").unwrap().is_array());
    }

    #[test]
    fn state_changed_push_round_trips() {
        let p = StateChangedParams {
            snapshot: StateSnapshot {
                pc_id: "PC1234".into(),
                online: true,
                checks: vec![],
                agent_version: "0.4.0".into(),
                target_version: "0.4.0".into(),
            },
            at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: StateChangedParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.snapshot.pc_id, "PC1234");
    }
}
