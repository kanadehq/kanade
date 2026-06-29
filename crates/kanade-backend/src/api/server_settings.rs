//! Backend-side, operator-editable server settings (`server_settings` KV
//! bucket).
//!
//!   GET /api/server-settings           (viewer+) -> ServerSettings (stored)
//!   GET /api/server-settings/defaults  (viewer+) -> ServerSettings (built-in)
//!   PUT /api/server-settings           (operator) replace the whole document
//!
//! A single KV singleton ([`BUCKET_SERVER_SETTINGS`] /
//! [`KEY_SERVER_SETTINGS`]) holds the current [`ServerSettings`]. Unlike
//! `fleet_config` (every agent watches it) this is read backend-side only
//! — currently by the cleanup task for the dead-agent prune window. A
//! deliberately generic document so future server knobs join the same key
//! and Settings tab rather than spawning a bucket each. A missing key ⇒
//! all-default (e.g. pruning disabled), so a fresh deployment behaves as
//! it did before the bucket existed.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kanade_shared::kv::{BUCKET_SERVER_SETTINGS, KEY_SERVER_SETTINGS};
use kanade_shared::wire::{MAX_AGENT_PRUNE_DAYS, ServerSettings};
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

/// `PUT /api/server-settings` — replace the whole server-settings
/// document (full-replace, not a per-field PATCH). Each field the body
/// omits decodes to its `None`/default, so a caller should send the
/// complete document it knows; the SPA always does. (Cross-version
/// merge/CAS is deferred until a second field lands — see #884.)
///
/// Validates `agent_prune_days` before storing: `Some(0)` is rejected
/// (use `null` / omit to disable — storing `0` would round-trip back and
/// wedge the SPA's `min=1` field), and a value over
/// [`MAX_AGENT_PRUNE_DAYS`] is rejected (it would otherwise have to be
/// clamped silently, and it signals a client bug).
pub async fn put(
    State(s): State<AppState>,
    caller: Caller,
    Json(settings): Json<ServerSettings>,
) -> Result<Json<ServerSettings>, (StatusCode, String)> {
    if let Some(days) = settings.agent_prune_days {
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
    let kv = open_bucket(&s).await?;
    let body = serde_json::to_vec(&settings).map_err(|e| {
        warn!(error = %e, "encode server_settings");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode server_settings: {e}"),
        )
    })?;
    kv.put(KEY_SERVER_SETTINGS, body.into())
        .await
        .map_err(|e| {
            warn!(error = %e, "write server_settings");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write server_settings: {e}"),
            )
        })?;
    info!(
        agent_prune_days = ?settings.agent_prune_days,
        "server_settings replaced",
    );
    audit::record(
        &s.nats,
        "operator",
        "server_settings_set",
        Some(KEY_SERVER_SETTINGS),
        Some(&caller),
        // Capture the whole document so the audit entry stays complete
        // automatically as ServerSettings grows (e.g. #884's SMTP fields).
        serde_json::to_value(&settings).unwrap_or(serde_json::Value::Null),
    )
    .await;
    Ok(Json(settings))
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
