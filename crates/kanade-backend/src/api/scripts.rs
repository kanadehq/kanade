//! Script lifecycle endpoints — flip a command id's status in the
//! `script_status` KV bucket between ACTIVE and REVOKED so agents
//! that haven't yet run the cmd skip it (spec §2.6 Layer 2). Maps
//! 1:1 to the operator-side `kanade revoke` / `kanade unrevoke`
//! subcommands.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::TryStreamExt;
use kanade_shared::kv::{BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_ACTIVE, SCRIPT_STATUS_REVOKED};
use tracing::{info, warn};

use super::AppState;
use crate::audit;
use crate::audit::Caller;

/// GET /api/scripts/status — snapshot of the `script_status` KV
/// bucket: `{ cmd_id: "ACTIVE" | "REVOKED" }`. The SPA's Jobs page
/// uses this to badge revoked manifests inline (so an operator can
/// tell at a glance whether the revoke / unrevoke button click
/// actually landed) and to disable redundant clicks (revoke when
/// already REVOKED is a no-op put, but the UI shouldn't suggest it
/// as a meaningful action).
///
/// Returns an empty map when the bucket is missing rather than
/// erroring — useful in fresh / partial bootstrap setups where the
/// KV hasn't been provisioned yet (the SPA can still render Jobs
/// without status info; nothing is shown as revoked, which is the
/// safe default).
pub async fn list_status(
    State(state): State<AppState>,
) -> Result<Json<HashMap<String, String>>, (StatusCode, String)> {
    let kv = match state.jetstream.get_key_value(BUCKET_SCRIPT_STATUS).await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "script_status KV bucket missing on list");
            return Ok(Json(HashMap::new()));
        }
    };
    let keys_stream = kv.keys().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("script_status KV keys: {e}"),
        )
    })?;
    let keys: Vec<String> = keys_stream.try_collect().await.unwrap_or_default();
    // Fan the per-key GETs out in parallel — sequential .get().await
    // serialised every round-trip and scaled linearly in catalog
    // size. join_all here is bounded by the operator-facing jobs
    // catalog (~10s-100s in practice), well within NATS connection
    // capacity. Gemini #47 review.
    let fetches = keys.into_iter().map(|k| {
        let kv = kv.clone();
        async move {
            match kv.get(&k).await {
                Ok(Some(bytes)) => Some((k, String::from_utf8_lossy(&bytes).into_owned())),
                _ => None,
            }
        }
    });
    let out: HashMap<String, String> = futures::future::join_all(fetches)
        .await
        .into_iter()
        .flatten()
        .collect();
    Ok(Json(out))
}

pub async fn revoke(
    State(state): State<AppState>,
    Path(cmd_id): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    set_status(&state, &cmd_id, SCRIPT_STATUS_REVOKED).await?;
    info!(cmd_id = %cmd_id, "marked REVOKED");
    audit::record(
        &state.nats,
        "operator",
        "script_revoke",
        Some(&cmd_id),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unrevoke(
    State(state): State<AppState>,
    Path(cmd_id): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    set_status(&state, &cmd_id, SCRIPT_STATUS_ACTIVE).await?;
    info!(cmd_id = %cmd_id, "marked ACTIVE");
    audit::record(
        &state.nats,
        "operator",
        "script_unrevoke",
        Some(&cmd_id),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_status(
    state: &AppState,
    cmd_id: &str,
    value: &str,
) -> Result<(), (StatusCode, String)> {
    let kv = state
        .jetstream
        .get_key_value(BUCKET_SCRIPT_STATUS)
        .await
        .map_err(|e| {
            warn!(error = %e, "open script_status KV bucket");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("script_status KV bucket unavailable: {e}"),
            )
        })?;
    kv.put(cmd_id, bytes::Bytes::copy_from_slice(value.as_bytes()))
        .await
        .map_err(|e| {
            warn!(error = %e, cmd_id, "write script_status");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write script_status for {cmd_id}: {e}"),
            )
        })?;
    Ok(())
}
