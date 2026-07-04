//! Server-level operator actions (as opposed to editable settings — those
//! live in `server_settings`).
//!
//!   POST /api/server/restart   (operator) restart the backend service
//!
//! The backend restarts **itself** rather than dispatching a job to the
//! co-located agent: it exits with a non-zero code, and the Windows SCM's
//! configured failure-recovery actions relaunch the service a few seconds
//! later (`sc.exe failure … actions= restart/5000/… ` + `failureflag 1`,
//! set by `scripts/deploy/backend.ps1`). This is the exact mechanism the
//! boot sentinel already uses to restart after a rollback (`exit(1)` in
//! `main`), so there's no agent, no NATS command, and no "which pc_id am I"
//! discovery to get wrong. Motivating use: applying a `server_settings` mail
//! (SMTP) change, which the backend only reads at startup (#884 / #962).

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use tracing::warn;

use crate::api::AppState;
use crate::audit;
use crate::audit::Caller;

/// How long the handler waits after responding before exiting, so the `202`
/// flushes to the operator's browser before the process dies. Short — just
/// enough to lose the race against the response write.
///
/// This is a heuristic, not a guarantee: it assumes the SPA talks to the
/// backend directly (the deployment topology here — a LAN host serving
/// `:8080`, no buffering reverse proxy in front). Behind a proxy that buffers
/// the response, 750 ms could be too short and the operator would see a
/// connection error even though the restart was accepted — cosmetic (the SPA
/// polls `/health` and reloads when the service is back), not a correctness
/// issue. Likewise `std::process::exit(1)` is a deliberate, non-graceful
/// process-wide kill: any other in-flight request is dropped, matching the
/// boot sentinel's `exit(1)` restart contract (a graceful drain isn't worth
/// the machinery for an operator-initiated, already-confirmed restart).
const RESTART_GRACE: Duration = Duration::from_millis(750);

/// Body of the `202 Accepted` reply to a restart request.
#[derive(Serialize)]
pub struct RestartAck {
    /// Always `"restarting"`. The SPA shows a "server is coming back"
    /// state and re-polls once the process is gone.
    status: &'static str,
}

/// `POST /api/server/restart` — restart the backend service (operator).
///
/// Audit-logs the request, schedules a delayed `std::process::exit(1)`, and
/// returns `202` immediately. The non-zero exit trips the SCM failure-
/// recovery actions, which relaunch the service (~5 s). The HTTP API is
/// briefly unavailable in between — the SPA button confirms first and shows
/// a hint.
///
/// Dev/console path (no SCM): the process just exits and does **not** come
/// back — restart it by hand. In production the service always runs under
/// SCM with the recovery actions configured, so it returns on its own.
pub async fn restart(State(s): State<AppState>, caller: Caller) -> (StatusCode, Json<RestartAck>) {
    audit::record(
        &s.nats,
        "operator",
        "server_restart",
        None,
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    warn!(
        grace = ?RESTART_GRACE,
        "operator-requested backend restart — exiting (1) for the SCM to relaunch the service",
    );
    // Detached: the request returns 202 right away, then the whole process
    // dies after the grace period. A non-zero exit is what the SCM's
    // failure-recovery actions restart on (a clean exit 0 would be treated
    // as a deliberate stop and NOT relaunched) — same contract as the boot
    // sentinel's `exit(1)`.
    tokio::spawn(async {
        tokio::time::sleep(RESTART_GRACE).await;
        std::process::exit(1);
    });
    (
        StatusCode::ACCEPTED,
        Json(RestartAck {
            status: "restarting",
        }),
    )
}
