//! Script lifecycle endpoints — flip a command id's status in the
//! `script_status` KV bucket between ACTIVE and REVOKED so agents
//! that haven't yet run the cmd skip it (spec §2.6 Layer 2). Maps
//! 1:1 to the operator-side `kanade revoke` / `kanade unrevoke`
//! subcommands.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use kanade_shared::kv::{BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_ACTIVE, SCRIPT_STATUS_REVOKED};
use tracing::{info, warn};

use super::AppState;

pub async fn revoke(
    State(state): State<AppState>,
    Path(cmd_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_status(&state, &cmd_id, SCRIPT_STATUS_REVOKED).await?;
    info!(cmd_id = %cmd_id, "marked REVOKED");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unrevoke(
    State(state): State<AppState>,
    Path(cmd_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_status(&state, &cmd_id, SCRIPT_STATUS_ACTIVE).await?;
    info!(cmd_id = %cmd_id, "marked ACTIVE");
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
