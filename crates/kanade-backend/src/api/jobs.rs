//! Job control endpoints. Currently just kill — publishing on
//! `kill.{job_id}` so any agent currently executing a Command with
//! that job_id terminates its child process (spec §2.6 Layer 3).
//! Maps 1:1 to `kanade kill <job_id>`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use kanade_shared::subject;
use tracing::{info, warn};

use super::AppState;

pub async fn kill(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .nats
        .publish(subject::kill(&job_id), bytes::Bytes::new())
        .await
        .map_err(|e| {
            warn!(error = %e, job_id, "publish kill");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("publish kill.{job_id}: {e}"),
            )
        })?;
    // flush so the subject is on the wire before we ack the
    // operator — without it, a fast operator-then-shutdown could
    // theoretically drop the kill on the floor.
    let _ = state.nats.flush().await;
    info!(job_id = %job_id, "kill signal published");
    Ok(StatusCode::NO_CONTENT)
}
