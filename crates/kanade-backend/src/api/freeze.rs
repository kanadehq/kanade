//! Fleet-wide change-freeze endpoints (#418 Phase 5).
//!
//! A single KV singleton ([`BUCKET_FLEET_CONFIG`] / [`KEY_FREEZE`])
//! holds the current [`Freeze`], or is absent when the fleet isn't
//! frozen. The backend scheduler and every agent's local scheduler
//! watch it and skip *all* fires while it's active. These routes are
//! the operator surface: read it, set it, clear it.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kanade_shared::kv::{BUCKET_FLEET_CONFIG, KEY_FREEZE};
use kanade_shared::manifest::Freeze;
use tracing::info;

use crate::api::AppState;
use crate::audit;
use crate::audit::Caller;

/// `GET /api/freeze` — the current fleet freeze, or `null` when the
/// fleet isn't frozen (the KV key is absent). Returns the stored
/// [`Freeze`] verbatim so the SPA can show the window + reason.
pub async fn get(State(s): State<AppState>) -> Result<Json<Option<Freeze>>, (StatusCode, String)> {
    // The bucket exists from bootstrap, so a lookup error means the
    // broker is unreachable — surface 500 rather than reporting "not
    // frozen", which would be a dangerous lie for a safety switch
    // (coderabbit #472).
    let kv = s
        .jetstream
        .get_key_value(BUCKET_FLEET_CONFIG)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("fleet_config bucket unreachable: {e}"),
            )
        })?;
    match kv.get(KEY_FREEZE).await {
        Ok(Some(bytes)) => match serde_json::from_slice::<Freeze>(&bytes) {
            Ok(freeze) => Ok(Json(Some(freeze))),
            // A corrupt blob is reported as "frozen, indeterminate" by
            // the schedulers (fail-safe), but the API surfaces the
            // decode error so an operator can fix or clear it.
            Err(e) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("stored freeze is corrupt: {e}"),
            )),
        },
        Ok(None) => Ok(Json(None)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}"))),
    }
}

/// `PUT /api/freeze` — set (or replace) the fleet freeze. An empty
/// body (`{}`) freezes indefinitely; `{ from, until }` freezes only
/// within that window. Validated like a schedule's `active` bounds.
pub async fn set(
    State(s): State<AppState>,
    caller: Caller,
    Json(freeze): Json<Freeze>,
) -> Result<Json<Freeze>, (StatusCode, String)> {
    if let Err(e) = freeze.validate() {
        return Err((StatusCode::BAD_REQUEST, format!("invalid freeze: {e}")));
    }
    // The bucket is provisioned once at bootstrap (`ensure_jetstream_
    // _resources`), so just attach — no per-request create (gemini /
    // claude #472: `create_key_value` on every PUT is redundant and
    // errors when the bucket exists with a different config). A real
    // NATS error here surfaces as 500 rather than being papered over.
    let kv = s
        .jetstream
        .get_key_value(BUCKET_FLEET_CONFIG)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("fleet_config bucket unreachable: {e}"),
            )
        })?;
    let body = serde_json::to_vec(&freeze)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
    kv.put(KEY_FREEZE, body.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;
    info!(
        from = ?freeze.from,
        until = ?freeze.until,
        reason = ?freeze.reason,
        "fleet change-freeze set",
    );
    audit::record(
        &s.nats,
        "operator",
        "freeze_set",
        Some(KEY_FREEZE),
        Some(&caller),
        serde_json::json!({
            "from": freeze.from,
            "until": freeze.until,
            "reason": freeze.reason,
        }),
    )
    .await;
    Ok(Json(freeze))
}

/// `DELETE /api/freeze` — clear the fleet freeze (thaw). A missing key
/// is already "not frozen", so deleting a nonexistent key is a no-op
/// success (idempotent thaw).
pub async fn clear(
    State(s): State<AppState>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    // Don't silently succeed on a NATS error (gemini #472): a failed
    // bucket lookup means the broker is unreachable, so the freeze may
    // NOT have been cleared — surface 500 instead of a misleading
    // 204. The bucket itself exists from bootstrap; a delete of an
    // absent key is an idempotent no-op (already "not frozen").
    let kv = s
        .jetstream
        .get_key_value(BUCKET_FLEET_CONFIG)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("fleet_config bucket unreachable: {e}"),
            )
        })?;
    kv.delete(KEY_FREEZE)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV delete: {e}")))?;
    info!("fleet change-freeze cleared");
    audit::record(
        &s.nats,
        "operator",
        "freeze_clear",
        Some(KEY_FREEZE),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
