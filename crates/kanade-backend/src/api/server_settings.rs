//! Backend-side, operator-editable server settings (`server_settings` KV
//! bucket).
//!
//!   GET /api/server-settings           (viewer+) -> ServerSettings (stored)
//!   GET /api/server-settings/defaults  (viewer+) -> ServerSettings (built-in)
//!   PUT /api/server-settings           (operator) per-field merge (PATCH)
//!
//! A single KV singleton ([`BUCKET_SERVER_SETTINGS`] /
//! [`KEY_SERVER_SETTINGS`]) holds the current [`ServerSettings`]. Unlike
//! `fleet_config` (every agent watches it) this is read backend-side only
//! — the cleanup task (dead-agent prune window), the controller-tier
//! dispatch guard, and the startup `Mailer` build (SMTP config, #884). A
//! deliberately generic document so future server knobs join the same key
//! and Settings tab rather than spawning a bucket each. A missing key ⇒
//! all-default (e.g. pruning disabled, email off), so a fresh deployment
//! behaves as it did before the bucket existed.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kanade_shared::config::MailSection;
use kanade_shared::kv::{BUCKET_SERVER_SETTINGS, KEY_SERVER_SETTINGS};
use kanade_shared::kv_cas;
use kanade_shared::wire::{MAX_AGENT_PRUNE_DAYS, ServerSettings};
use lettre::message::Mailbox;
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::api::AppState;
use crate::audit;
use crate::audit::Caller;

/// `GET /api/server-settings` — the current server settings, or all-default
/// when the key is absent.
pub async fn get(State(s): State<AppState>) -> Result<Json<ServerSettings>, (StatusCode, String)> {
    let kv = open_bucket(&s).await?;
    // A missing key is the normal "never configured" state → defaults.
    // A decode error is surfaced (not papered over with defaults) so an
    // operator can fix a corrupt document rather than silently running
    // with pruning off when they thought it was on.
    match kv.get(KEY_SERVER_SETTINGS).await {
        Ok(Some(bytes)) => serde_json::from_slice::<ServerSettings>(&bytes)
            .map(Json)
            .map_err(|e| {
                warn!(error = %e, "decode server_settings");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("stored server_settings is corrupt: {e}"),
                )
            }),
        Ok(None) => Ok(Json(ServerSettings::default())),
        Err(e) => {
            warn!(error = %e, "read server_settings");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read server_settings: {e}"),
            ))
        }
    }
}

/// `GET /api/server-settings/defaults` — the compiled-in default each
/// field falls back to when left unset, sourced from the Rust source of
/// truth so the SPA's faint placeholders never drift from it (mirrors
/// `/api/config/defaults`). No KV round-trip: defaults are static.
pub async fn defaults() -> Json<ServerSettings> {
    Json(ServerSettings::defaults())
}

/// `PUT /api/server-settings` — **per-field merge** (PATCH semantics), not
/// a full-document replace. Only the keys present in the request body are
/// touched: a key sent with a value overwrites it, a key sent as `null`
/// unsets it (the field falls back to its default), and a key the body
/// omits entirely is left exactly as it was.
///
/// This matters now that the document has more than one field (#884, from a
/// CodeRabbit review of #886): a full replace would let a client that
/// predates a field silently clear it just by not sending it. The merge is
/// done at the raw-JSON level so a field this build doesn't even know about
/// (written by a newer backend) survives untouched — a typed round-trip
/// would drop it on decode. The read-merge-write runs under KV
/// optimistic-concurrency ([`kv_cas::read_modify_write`]): a concurrent
/// editor can't clobber this one's change (the CAS re-reads and re-applies
/// on a revision conflict instead of blind-`put`ting a stale snapshot).
///
/// Validation (before any write):
/// - `agent_prune_days`: `Some(0)` rejected (omit / `null` to disable — a
///   stored `0` would round-trip and wedge the SPA's `min=1` field); over
///   [`MAX_AGENT_PRUNE_DAYS`] rejected (signals a client bug).
/// - `controller_group`: present-but-blank rejected (unambiguous stored doc).
/// - `mail`: host non-empty, port in 1..=65535, `from` a parseable address
///   (the same parser `Mailer` uses at boot, so a bad address is caught here
///   instead of silently disabling email at the next restart).
pub async fn put(
    State(s): State<AppState>,
    caller: Caller,
    Json(incoming): Json<Value>,
) -> Result<Json<ServerSettings>, (StatusCode, String)> {
    let incoming = match incoming {
        Value::Object(m) => m,
        _ => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "server_settings body must be a JSON object".to_string(),
            ));
        }
    };

    // Decode a typed view purely to validate + normalise the fields THIS
    // build knows. Unknown keys are ignored here but preserved by the merge
    // below. `null`s decode to `None` (= unset), which the merge maps to
    // "remove the key".
    let typed: ServerSettings =
        serde_json::from_value(Value::Object(incoming.clone())).map_err(|e| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid server_settings body: {e}"),
            )
        })?;
    validate(&typed)?;
    let typed = normalize(typed);

    // Precompute the normalised JSON values once (outside the CAS closure,
    // which may re-run on a revision conflict and can't be fallible).
    let prune_value = typed.agent_prune_days.map(Value::from);
    let controller_value = typed.controller_group.clone().map(Value::String);
    let mail_value = match typed.mail.as_ref() {
        Some(m) => Some(serde_json::to_value(m).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("encode mail settings: {e}"),
            )
        })?),
        None => None,
    };

    let kv = open_bucket(&s).await?;
    // Merge under optimistic concurrency: read the current document (raw, so
    // unknown fields survive), apply only the addressed keys, and CAS-write —
    // retrying the whole round on a revision conflict. `read_modify_write`
    // decodes a missing key to an empty map (first-ever write).
    let merged_map =
        kv_cas::read_modify_write::<Map<String, Value>, _>(&kv, KEY_SERVER_SETTINGS, |obj| {
            let mut changed = false;
            changed |= merge_field(obj, &incoming, "agent_prune_days", prune_value.clone());
            changed |= merge_field(obj, &incoming, "controller_group", controller_value.clone());
            changed |= merge_field(obj, &incoming, "mail", mail_value.clone());
            changed
        })
        .await
        .map_err(|e| {
            warn!(error = %format!("{e:#}"), "write server_settings");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write server_settings: {e}"),
            )
        })?;

    let doc = Value::Object(merged_map);
    // Decode the merged document for the response (drops any unknown keys,
    // which the client can't act on) and to log/audit the resulting state.
    let merged: ServerSettings = serde_json::from_value(doc.clone()).map_err(|e| {
        warn!(error = %e, "decode merged server_settings");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("merged server_settings is corrupt: {e}"),
        )
    })?;
    info!(
        agent_prune_days = ?merged.agent_prune_days,
        controller_group = ?merged.controller_group,
        mail_configured = merged.mail.is_some(),
        "server_settings merged",
    );
    audit::record(
        &s.nats,
        "operator",
        "server_settings_set",
        Some(KEY_SERVER_SETTINGS),
        Some(&caller),
        // Audit the whole stored document (raw, so it stays complete even
        // for keys this build doesn't model). The SMTP password is never in
        // the document, so this can't leak it.
        doc,
    )
    .await;
    Ok(Json(merged))
}

/// Overwrite / unset / preserve one key of the stored document per the
/// request body. `incoming` is the raw request object (used only to test
/// key *presence*); `value` is the normalised value the field decoded to
/// (`None` = the key was sent as `null`).
///
/// - key absent from the request  → leave the stored value untouched.
/// - key present with a value      → overwrite.
/// - key present as `null`         → remove (unset; falls back to default).
///
/// Returns whether the map actually changed, so the CAS closure can skip a
/// no-op write (which would bump the revision and wake watchers for nothing).
fn merge_field(
    obj: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
    key: &str,
    value: Option<Value>,
) -> bool {
    if !incoming.contains_key(key) {
        return false;
    }
    match value {
        Some(v) => {
            if obj.get(key) == Some(&v) {
                return false;
            }
            obj.insert(key.to_string(), v);
            true
        }
        None => obj.remove(key).is_some(),
    }
}

/// Reject a body whose known fields are out of range. Runs on the decoded
/// typed view before anything is written.
fn validate(s: &ServerSettings) -> Result<(), (StatusCode, String)> {
    if let Some(days) = s.agent_prune_days {
        if days == 0 {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "agent_prune_days must be >= 1; omit it or send null to disable pruning"
                    .to_string(),
            ));
        }
        if days > MAX_AGENT_PRUNE_DAYS {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("agent_prune_days must be <= {MAX_AGENT_PRUNE_DAYS} (100 years)"),
            ));
        }
    }
    // A present-but-blank controller_group would round-trip as "set but
    // empty", which the dispatch guard treats as unset anyway — reject it so
    // the stored document is unambiguous (omit / null to unset).
    if let Some(g) = s.controller_group.as_deref()
        && g.trim().is_empty()
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "controller_group must be a non-empty group name; omit it or send null to unset"
                .to_string(),
        ));
    }
    if let Some(m) = s.mail.as_ref() {
        validate_mail(m)?;
    }
    Ok(())
}

/// Validate the non-secret SMTP settings mirror what `Mailer::from_config`
/// needs, so a save can't produce a config that silently fails to build a
/// mailer at the next restart.
fn validate_mail(m: &MailSection) -> Result<(), (StatusCode, String)> {
    if m.host.trim().is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "mail.host must not be empty".to_string(),
        ));
    }
    if m.port == 0 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "mail.port must be between 1 and 65535".to_string(),
        ));
    }
    if m.from.trim().parse::<Mailbox>().is_err() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("mail.from is not a valid email address: {:?}", m.from),
        ));
    }
    Ok(())
}

/// Trim string fields to their canonical stored form so a re-read of an
/// unedited value never reads as "dirty" and the backend consumers (which
/// also trim) see the same bytes. A mail `username` that trims to empty
/// becomes `None` (an unauthenticated relay), matching `Mailer::from_config`.
fn normalize(mut s: ServerSettings) -> ServerSettings {
    if let Some(g) = s.controller_group.as_mut() {
        *g = g.trim().to_string();
    }
    if let Some(m) = s.mail.as_mut() {
        m.host = m.host.trim().to_string();
        m.from = m.from.trim().to_string();
        // Trim the username; a whitespace-only one collapses to `None`
        // (an unauthenticated relay), matching `Mailer::from_config`.
        m.username = m
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(String::from);
    }
    s
}

/// Read the stored [`ServerSettings`] backend-side (cleanup task,
/// controller-tier dispatch guard, …). A missing key ⇒ all-default; a
/// broker/decode failure is an `Err` so a security-sensitive caller (the
/// controller guard) can fail **closed** rather than acting on a guessed
/// default.
pub(crate) async fn load(s: &AppState) -> anyhow::Result<ServerSettings> {
    load_from_js(&s.jetstream).await
}

/// Read the stored [`ServerSettings`] from a raw JetStream context. Used at
/// startup — before `AppState` exists — to build the `Mailer` from the KV
/// mail config (#884). A missing key ⇒ all-default.
pub(crate) async fn load_from_js(
    js: &async_nats::jetstream::Context,
) -> anyhow::Result<ServerSettings> {
    use anyhow::Context;
    let kv = js
        .get_key_value(BUCKET_SERVER_SETTINGS)
        .await
        .context("open server_settings KV")?;
    match kv
        .get(KEY_SERVER_SETTINGS)
        .await
        .context("get server_settings")?
    {
        Some(bytes) => serde_json::from_slice(&bytes).context("decode server_settings"),
        None => Ok(ServerSettings::default()),
    }
}

/// Attach to the `server_settings` bucket. It's provisioned once at
/// bootstrap, so a lookup error means the broker is unreachable — surface
/// 503 rather than reporting defaults (which, for a destructive prune
/// knob, would be a misleading "off").
async fn open_bucket(
    s: &AppState,
) -> Result<async_nats::jetstream::kv::Store, (StatusCode, String)> {
    s.jetstream
        .get_key_value(BUCKET_SERVER_SETTINGS)
        .await
        .map_err(|e| {
            warn!(error = %e, bucket = BUCKET_SERVER_SETTINGS, "open server_settings KV bucket");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("server_settings KV bucket unavailable: {e}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use kanade_shared::config::{MailEncryption, MailSection};
    use serde_json::{Map, Value, json};

    use super::{ServerSettings, merge_field, normalize, validate};

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("object literal").clone()
    }

    fn sample_mail() -> MailSection {
        MailSection {
            host: "smtp.example.com".into(),
            port: 587,
            encryption: MailEncryption::Starttls,
            from: "kanade-noreply@example.com".into(),
            username: None,
        }
    }

    #[test]
    fn merge_key_absent_is_left_untouched() {
        // The request doesn't mention "a" → its stored value survives even
        // though we pass a would-be replacement, and it reports "no change".
        let mut stored = obj(json!({ "a": 1, "unknown_future": true }));
        assert!(!merge_field(
            &mut stored,
            &obj(json!({})),
            "a",
            Some(json!(2))
        ));
        assert_eq!(stored.get("a"), Some(&json!(1)));
        // And an unrelated (unknown to this build) key is never touched.
        assert_eq!(stored.get("unknown_future"), Some(&json!(true)));
    }

    #[test]
    fn merge_present_value_overwrites() {
        let mut stored = obj(json!({ "a": 1 }));
        assert!(merge_field(
            &mut stored,
            &obj(json!({ "a": 2 })),
            "a",
            Some(json!(2))
        ));
        assert_eq!(stored.get("a"), Some(&json!(2)));
    }

    #[test]
    fn merge_present_same_value_is_noop() {
        // Re-sending an unchanged value must report "no change" so the CAS
        // closure can skip a revision-bumping no-op write.
        let mut stored = obj(json!({ "a": 1 }));
        assert!(!merge_field(
            &mut stored,
            &obj(json!({ "a": 1 })),
            "a",
            Some(json!(1))
        ));
        assert_eq!(stored.get("a"), Some(&json!(1)));
    }

    #[test]
    fn merge_present_null_unsets() {
        // A key sent as null (typed value None) is removed so it falls back
        // to its default rather than being stored as an explicit null.
        let mut stored = obj(json!({ "a": 1 }));
        assert!(merge_field(
            &mut stored,
            &obj(json!({ "a": Value::Null })),
            "a",
            None
        ));
        assert!(!stored.contains_key("a"));
        // Removing an already-absent key is a no-op.
        assert!(!merge_field(
            &mut stored,
            &obj(json!({ "a": Value::Null })),
            "a",
            None
        ));
    }

    #[test]
    fn validate_rejects_zero_prune_days() {
        let s = ServerSettings {
            agent_prune_days: Some(0),
            ..Default::default()
        };
        assert!(validate(&s).is_err());
    }

    #[test]
    fn validate_rejects_blank_controller_group() {
        let s = ServerSettings {
            controller_group: Some("   ".into()),
            ..Default::default()
        };
        assert!(validate(&s).is_err());
    }

    #[test]
    fn validate_rejects_bad_mail() {
        // Empty host.
        let mut m = sample_mail();
        m.host = "".into();
        assert!(
            validate(&ServerSettings {
                mail: Some(m),
                ..Default::default()
            })
            .is_err()
        );
        // Port 0.
        let mut m = sample_mail();
        m.port = 0;
        assert!(
            validate(&ServerSettings {
                mail: Some(m),
                ..Default::default()
            })
            .is_err()
        );
        // Unparseable from-address.
        let mut m = sample_mail();
        m.from = "not an address".into();
        assert!(
            validate(&ServerSettings {
                mail: Some(m),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn validate_accepts_good_mail() {
        assert!(
            validate(&ServerSettings {
                mail: Some(sample_mail()),
                ..Default::default()
            })
            .is_ok()
        );
    }

    #[test]
    fn normalize_trims_and_blanks_username() {
        let mut m = sample_mail();
        m.host = "  smtp.example.com  ".into();
        m.from = "  kanade-noreply@example.com ".into();
        m.username = Some("   ".into());
        let s = normalize(ServerSettings {
            controller_group: Some("  infra ".into()),
            mail: Some(m),
            ..Default::default()
        });
        assert_eq!(s.controller_group.as_deref(), Some("infra"));
        let m = s.mail.unwrap();
        assert_eq!(m.host, "smtp.example.com");
        assert_eq!(m.from, "kanade-noreply@example.com");
        // A whitespace-only username becomes None (unauthenticated relay).
        assert_eq!(m.username, None);
    }
}
