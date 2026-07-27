//! Backend-side, operator-editable server settings (`server_settings` KV
//! bucket).
//!
//!   GET    /api/server-settings                        (viewer+) -> ServerSettings (stored)
//!   GET    /api/server-settings/defaults               (viewer+) -> ServerSettings (built-in)
//!   PUT    /api/server-settings                        (operator) per-field merge (PATCH)
//!   PUT    /api/server-settings/support-codes/{scope}  (operator) set/rotate one code
//!   DELETE /api/server-settings/support-codes/{scope}  (operator) remove one code
//!
//! Support codes get their own endpoints rather than riding the generic
//! merge, because a secret behaves differently from every other field here:
//! it is **write-only**. Responses blank the stored hash
//! ([`ServerSettings::redacted`]), so the document the SPA holds cannot be
//! sent back intact — routing it through the PATCH merge would let a
//! well-behaved "save the form" round-trip blank a live code. Set-and-forget
//! endpoints make that structurally impossible.
//!
//! A single KV singleton ([`BUCKET_SERVER_SETTINGS`] /
//! [`KEY_SERVER_SETTINGS`]) holds the current [`ServerSettings`]. Unlike
//! `fleet_config` (every agent watches it) this is read backend-side only
//! — the cleanup task (dead-agent prune window), the controller-tier
//! dispatch guard, the startup `Mailer` build (SMTP config, #884), and the
//! collect-bundle retention reconcile (the `collections` Object Store's
//! `max_age`, applied at boot and on save). A
//! deliberately generic document so future server knobs join the same key
//! and Settings tab rather than spawning a bucket each. A missing key ⇒
//! all-default (e.g. pruning disabled, email off), so a fresh deployment
//! behaves as it did before the bucket existed.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use kanade_shared::config::MailSection;
use kanade_shared::kv::{BUCKET_SERVER_SETTINGS, KEY_SERVER_SETTINGS};
use kanade_shared::kv_cas;
use kanade_shared::wire::{
    MAX_AGENT_PRUNE_DAYS, MAX_CHECK_STATUS_STALE_DAYS, MAX_COLLECT_RETENTION_DAYS,
    MAX_SESSION_TTL_HOURS, MAX_SUPPORT_UNLOCK_TTL_MINUTES, ServerSettings, SupportCode,
};
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
            // Blank the support-code hashes: this route is viewer+, so an
            // unredacted response would hand every read-only account an
            // offline-crackable hash for a code that grants privileged job
            // execution on end-user machines.
            .map(|s| Json(s.redacted()))
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
/// - `collect_retention_days`: `Some(0)` rejected (omit / `null` to fall back
///   to the built-in default); over [`MAX_COLLECT_RETENTION_DAYS`] rejected.
/// - `session_ttl_hours`: `Some(0)` rejected (omit / `null` to fall back to
///   the built-in default); over [`MAX_SESSION_TTL_HOURS`] rejected.
/// - `check_status_stale_days`: `Some(0)` is **valid** (disables staleness —
///   show every row); only over [`MAX_CHECK_STATUS_STALE_DAYS`] is rejected.
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
    let collect_value = typed.collect_retention_days.map(Value::from);
    let session_ttl_value = typed.session_ttl_hours.map(Value::from);
    let check_stale_value = typed.check_status_stale_days.map(Value::from);
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
    // Whether the merge actually changed `collect_retention_days` — set inside
    // the CAS closure so it reflects the *committed* attempt (the closure is
    // re-run on a revision conflict; its last invocation is the one that
    // sticks). Gates the Object Store reconcile below so a save that merely
    // re-sends an unchanged value (the SPA always sends the full document)
    // doesn't pay a stream round-trip.
    let mut collect_changed = false;
    // Merge under optimistic concurrency: read the current document (raw, so
    // unknown fields survive), apply only the addressed keys, and CAS-write —
    // retrying the whole round on a revision conflict. `read_modify_write`
    // decodes a missing key to an empty map (first-ever write).
    let merged_map =
        kv_cas::read_modify_write::<Map<String, Value>, _>(&kv, KEY_SERVER_SETTINGS, |obj| {
            let mut changed = false;
            changed |= merge_field(obj, &incoming, "agent_prune_days", prune_value.clone());
            collect_changed = merge_field(
                obj,
                &incoming,
                "collect_retention_days",
                collect_value.clone(),
            );
            changed |= collect_changed;
            changed |= merge_field(
                obj,
                &incoming,
                "session_ttl_hours",
                session_ttl_value.clone(),
            );
            changed |= merge_field(
                obj,
                &incoming,
                "check_status_stale_days",
                check_stale_value.clone(),
            );
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
        collect_retention_days = ?merged.collect_retention_days,
        session_ttl_hours = ?merged.session_ttl_hours,
        controller_group = ?merged.controller_group,
        mail_configured = merged.mail.is_some(),
        "server_settings merged",
    );
    // Apply a collect-retention change to the live `collections` Object Store
    // right away (the bucket's max_age is broker-side, not re-read per request),
    // so the operator doesn't have to wait for a restart. Only when the merge
    // actually changed the value — reconcile is idempotent, but gating on the
    // real change avoids a stream round-trip on every unrelated save (mail,
    // prune, …), which matters because the SPA always sends the full document.
    // Best-effort: a failure is logged, not surfaced — the value is persisted
    // and the boot reconcile re-applies it on the next restart.
    if collect_changed {
        let days = merged.effective_collect_retention_days();
        match kanade_shared::bootstrap::reconcile_collect_retention(&s.jetstream, days).await {
            Ok(true) => info!(
                collect_retention_days = days,
                "collect retention applied to Object Store"
            ),
            Ok(false) => {}
            Err(e) => warn!(
                error = %format!("{e:#}"), collect_retention_days = days,
                "collect retention: applied to KV but reconcile of the Object Store max_age failed; \
                 will be applied on the next backend restart",
            ),
        }
    }
    audit::record(
        &s.nats,
        "operator",
        "server_settings_set",
        Some(KEY_SERVER_SETTINGS),
        Some(&caller),
        // Audit the whole stored document (raw, so it stays complete even
        // for keys this build doesn't model) minus the support-code hashes.
        // The SMTP password is never in the document; the hashes are, and an
        // audit trail is a long-lived, widely-readable copy — exactly what a
        // secret must not be duplicated into.
        redact_support_hashes(doc),
    )
    .await;
    Ok(Json(merged.redacted()))
}

/// Blank every `support_codes[].hash` in a raw settings document, leaving
/// the rest byte-identical. Used on the audit copy, which is raw JSON (so
/// unknown keys survive) rather than the typed view [`ServerSettings::redacted`]
/// operates on.
fn redact_support_hashes(mut doc: Value) -> Value {
    if let Some(codes) = doc.get_mut("support_codes").and_then(Value::as_array_mut) {
        for c in codes {
            if let Some(obj) = c.as_object_mut() {
                obj.remove("hash");
            }
        }
    }
    doc
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
    if let Some(days) = s.collect_retention_days {
        // `Some(0)` is rejected for the same reason as agent_prune_days: it
        // would round-trip and wedge the SPA's `min=1` field. Omit / `null`
        // to fall back to the built-in default instead.
        if days == 0 {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "collect_retention_days must be >= 1; omit it or send null to use the default"
                    .to_string(),
            ));
        }
        if days > MAX_COLLECT_RETENTION_DAYS {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "collect_retention_days must be <= {MAX_COLLECT_RETENTION_DAYS} (10 years)"
                ),
            ));
        }
    }
    if let Some(hours) = s.session_ttl_hours {
        // `Some(0)` is rejected like the other numeric knobs: it would
        // round-trip and wedge the SPA's `min=1` field, and a 0-hour token is
        // expired the instant it's minted. Omit / `null` to fall back to the
        // built-in 24h default instead.
        if hours == 0 {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "session_ttl_hours must be >= 1; omit it or send null to use the default"
                    .to_string(),
            ));
        }
        if hours > MAX_SESSION_TTL_HOURS {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("session_ttl_hours must be <= {MAX_SESSION_TTL_HOURS} (365 days)"),
            ));
        }
    }
    if let Some(days) = s.check_status_stale_days {
        // Unlike the other numeric knobs, `0` is VALID here — it disables
        // staleness (every check_status row is shown). So only the upper cap is
        // enforced; the SPA field allows 0.
        if days > MAX_CHECK_STATUS_STALE_DAYS {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "check_status_stale_days must be <= {MAX_CHECK_STATUS_STALE_DAYS} (10 years)"
                ),
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
/// Body of `PUT /api/server-settings/support-codes/{scope}`.
///
/// `code` is the plaintext the operator typed. It is hashed here and
/// **never stored, logged, echoed, or audited** — the response is the
/// redacted settings document, so there is no path by which it comes back.
#[derive(serde::Deserialize)]
pub struct SupportCodeBody {
    /// The secret the helpdesk will type into the Client App.
    pub code: String,
    /// Human label shown in the Client App's support-mode banner.
    #[serde(default)]
    pub label: Option<String>,
    /// Grant window in minutes. Omit for
    /// [`DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES`].
    #[serde(default)]
    pub ttl_minutes: Option<u32>,
    /// Suspend the code without deleting it (the secret stays set, so
    /// re-enabling doesn't require re-issuing one).
    #[serde(default)]
    pub disabled: bool,
}

/// Minimum length for a support code. Short enough not to fight an operator
/// choosing something typable over the phone, long enough that the
/// per-machine rate limit (5 tries / 5 min) makes guessing hopeless rather
/// than merely slow.
const MIN_SUPPORT_CODE_LEN: usize = 8;

/// `PUT /api/server-settings/support-codes/{scope}` — set or rotate the code
/// for one unlock scope (operator+).
///
/// Upsert by scope: setting a code for an existing scope replaces the hash
/// (a rotation) and leaves nothing recoverable of the old one. The write
/// runs under the same KV optimistic-concurrency as the generic PUT, so it
/// can't clobber a concurrent settings edit.
pub async fn put_support_code(
    State(s): State<AppState>,
    caller: Caller,
    Path(scope): Path<String>,
    Json(body): Json<SupportCodeBody>,
) -> Result<Json<ServerSettings>, (StatusCode, String)> {
    let scope = scope.trim().to_string();
    validate_support_code(&scope, &body)?;

    // Hash before the CAS: argon2 is deliberately slow, and the CAS closure
    // may re-run on a revision conflict.
    let hash = hash_support_code(&body.code)?;
    let entry = SupportCode {
        scope: scope.clone(),
        hash,
        label: body.label.map(|l| l.trim().to_string()),
        ttl_minutes: body.ttl_minutes,
        disabled: body.disabled,
    };
    let entry_value = serde_json::to_value(&entry).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode support code: {e}"),
        )
    })?;

    let kv = open_bucket(&s).await?;
    let merged_map =
        kv_cas::read_modify_write::<Map<String, Value>, _>(&kv, KEY_SERVER_SETTINGS, |obj| {
            let codes = obj
                .entry("support_codes".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            // A non-array value here means the document was hand-corrupted;
            // replacing it is the only forward move (and loses nothing a
            // reader could have used — the agent would have failed to decode
            // the whole document).
            if !codes.is_array() {
                *codes = Value::Array(Vec::new());
            }
            let arr = codes.as_array_mut().expect("just ensured array");
            arr.retain(|c| c.get("scope").and_then(Value::as_str) != Some(scope.as_str()));
            arr.push(entry_value.clone());
            true
        })
        .await
        .map_err(|e| {
            warn!(error = %format!("{e:#}"), "write support code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write support code: {e}"),
            )
        })?;

    let merged = decode_merged(Value::Object(merged_map))?;
    info!(
        scope = %entry.scope,
        disabled = entry.disabled,
        "support code set",
    );
    audit::record(
        &s.nats,
        "operator",
        "support_code_set",
        Some(&entry.scope),
        Some(&caller),
        // Scope + knobs only: never the code, never its hash.
        serde_json::json!({
            "scope": entry.scope,
            "label": entry.label,
            "ttl_minutes": entry.ttl_minutes,
            "disabled": entry.disabled,
        }),
    )
    .await;
    Ok(Json(merged.redacted()))
}

/// `DELETE /api/server-settings/support-codes/{scope}` — remove one scope's
/// code (operator+). Idempotent: deleting a scope that has no code is a
/// success, since the end state the caller asked for is the state they get.
pub async fn delete_support_code(
    State(s): State<AppState>,
    caller: Caller,
    Path(scope): Path<String>,
) -> Result<Json<ServerSettings>, (StatusCode, String)> {
    let scope = scope.trim().to_string();
    let kv = open_bucket(&s).await?;
    let merged_map =
        kv_cas::read_modify_write::<Map<String, Value>, _>(&kv, KEY_SERVER_SETTINGS, |obj| {
            let Some(arr) = obj.get_mut("support_codes").and_then(Value::as_array_mut) else {
                return false;
            };
            let before = arr.len();
            arr.retain(|c| c.get("scope").and_then(Value::as_str) != Some(scope.as_str()));
            arr.len() != before
        })
        .await
        .map_err(|e| {
            warn!(error = %format!("{e:#}"), "delete support code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("delete support code: {e}"),
            )
        })?;

    let merged = decode_merged(Value::Object(merged_map))?;
    info!(scope = %scope, "support code deleted");
    audit::record(
        &s.nats,
        "operator",
        "support_code_deleted",
        Some(&scope),
        Some(&caller),
        serde_json::json!({ "scope": scope }),
    )
    .await;
    Ok(Json(merged.redacted()))
}

/// Reject a support-code body that could never work, before anything is
/// hashed or written.
fn validate_support_code(scope: &str, body: &SupportCodeBody) -> Result<(), (StatusCode, String)> {
    if !kanade_shared::manifest::is_valid_resource_id(scope) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "scope must be a slug ([A-Za-z0-9._-]) matching a job's client.unlock".to_string(),
        ));
    }
    // No trim: whitespace inside a secret is significant, so silently eating
    // the edges would store a different code than the operator typed. But
    // the Client App trims what the *user* types, so a code that needs an
    // edge space could never be redeemed — reject it here rather than ship
    // an unusable one.
    if body.code != body.code.trim() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "code must not start or end with whitespace".to_string(),
        ));
    }
    if body.code.chars().count() < MIN_SUPPORT_CODE_LEN {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("code must be at least {MIN_SUPPORT_CODE_LEN} characters"),
        ));
    }
    if let Some(ttl) = body.ttl_minutes {
        if ttl == 0 || ttl > MAX_SUPPORT_UNLOCK_TTL_MINUTES {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("ttl_minutes must be 1..={MAX_SUPPORT_UNLOCK_TTL_MINUTES}"),
            ));
        }
    }
    if let Some(label) = body.label.as_ref() {
        if label.trim().is_empty() {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "label must not be blank when set; omit it instead".to_string(),
            ));
        }
    }
    Ok(())
}

/// argon2id-hash a support code with a fresh random salt, in PHC format —
/// the same shape (and default parameters) the `users` table stores account
/// passwords in, so the agent's verifier needs no special casing.
fn hash_support_code(code: &str) -> Result<String, (StatusCode, String)> {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};

    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(code.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| {
            warn!(error = %e, "hash support code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to hash the support code".to_string(),
            )
        })
}

/// Decode a merged raw document back into the typed view for the response.
fn decode_merged(doc: Value) -> Result<ServerSettings, (StatusCode, String)> {
    serde_json::from_value(doc).map_err(|e| {
        warn!(error = %e, "decode merged server_settings");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("merged server_settings is corrupt: {e}"),
        )
    })
}

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

    use super::{
        MIN_SUPPORT_CODE_LEN, ServerSettings, SupportCodeBody, hash_support_code, merge_field,
        normalize, redact_support_hashes, validate, validate_support_code,
    };

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("object literal").clone()
    }

    fn code_body(code: &str) -> SupportCodeBody {
        SupportCodeBody {
            code: code.to_string(),
            label: None,
            ttl_minutes: None,
            disabled: false,
        }
    }

    #[test]
    fn support_code_validation_rejects_the_unusable() {
        assert!(validate_support_code("support", &code_body("hunter2!!")).is_ok());
        // Scope must be a slug — it is compared byte-for-byte with a
        // manifest's `client.unlock`, so anything else can never match.
        assert!(validate_support_code("has space", &code_body("hunter2!!")).is_err());
        assert!(validate_support_code("", &code_body("hunter2!!")).is_err());
        // Too short to survive guessing even at 5 tries / 5 min.
        let short = "a".repeat(MIN_SUPPORT_CODE_LEN - 1);
        assert!(validate_support_code("support", &code_body(&short)).is_err());
        // Edge whitespace: the client trims what the user types, so this
        // code could be stored but never redeemed.
        assert!(validate_support_code("support", &code_body(" hunter2!! ")).is_err());
        // Out-of-range TTL.
        let mut ttl = code_body("hunter2!!");
        ttl.ttl_minutes = Some(0);
        assert!(validate_support_code("support", &ttl).is_err());
        ttl.ttl_minutes = Some(u32::MAX);
        assert!(validate_support_code("support", &ttl).is_err());
        // Blank label (omit it instead).
        let mut label = code_body("hunter2!!");
        label.label = Some("   ".into());
        assert!(validate_support_code("support", &label).is_err());
    }

    #[test]
    fn hashed_code_verifies_and_is_salted() {
        use argon2::{Argon2, PasswordHash, PasswordVerifier};
        let a = hash_support_code("hunter2!!").unwrap();
        let b = hash_support_code("hunter2!!").unwrap();
        assert_ne!(a, b, "each hash must carry its own salt");
        for h in [&a, &b] {
            let parsed = PasswordHash::new(h).unwrap();
            assert!(
                Argon2::default()
                    .verify_password(b"hunter2!!", &parsed)
                    .is_ok()
            );
            assert!(
                Argon2::default()
                    .verify_password(b"hunter3!!", &parsed)
                    .is_err()
            );
        }
    }

    #[test]
    fn audit_copy_carries_no_hashes() {
        // The audit trail is a long-lived, widely-readable copy of the
        // document — the one place a secret must not be duplicated into.
        let doc = json!({
            "agent_prune_days": 7,
            "support_codes": [
                {"scope":"support","hash":"$argon2id$secret","label":"desk"},
                {"scope":"admin","hash":"$argon2id$other"},
            ],
        });
        let redacted = redact_support_hashes(doc);
        let text = redacted.to_string();
        assert!(!text.contains("argon2"), "audit leaked a hash: {text}");
        // Everything else survives, including the unrelated key.
        assert_eq!(redacted["agent_prune_days"], 7);
        assert_eq!(redacted["support_codes"][0]["scope"], "support");
        assert_eq!(redacted["support_codes"][0]["label"], "desk");
    }

    #[test]
    fn audit_redaction_tolerates_a_missing_or_odd_field() {
        // Documents written before the feature (no key) and hand-corrupted
        // ones (wrong type) must pass through rather than panic.
        assert_eq!(
            redact_support_hashes(json!({"agent_prune_days": 7}))["agent_prune_days"],
            7
        );
        assert_eq!(
            redact_support_hashes(json!({"support_codes": "nonsense"}))["support_codes"],
            "nonsense"
        );
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
    fn validate_rejects_zero_or_oversize_collect_retention() {
        use kanade_shared::wire::MAX_COLLECT_RETENTION_DAYS;
        assert!(
            validate(&ServerSettings {
                collect_retention_days: Some(0),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&ServerSettings {
                collect_retention_days: Some(MAX_COLLECT_RETENTION_DAYS + 1),
                ..Default::default()
            })
            .is_err()
        );
        // A value inside the range passes.
        assert!(
            validate(&ServerSettings {
                collect_retention_days: Some(90),
                ..Default::default()
            })
            .is_ok()
        );
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
