//! Operator-side action endpoints that drive a single agent through
//! NATS request/reply or short subscribe windows. Mirrors the CLI's
//! `kanade run` + `kanade ping` so the web UI can offer the same
//! interactive controls without shelling out to the CLI.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::{Command, ExecResult, Heartbeat, Shell};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use super::AppState;

const DEFAULT_RUN_TIMEOUT_SECS: u64 = 60;
const RESULT_WAIT_PADDING_SECS: u64 = 10;
const DEFAULT_PING_WAIT_SECS: u64 = 45;

/// Body of `POST /api/run`. Maps loosely to the YAML manifest's
/// inline-script form (spec §2.4.1) but with the request_id
/// generated server-side so the response is a clean ExecResult.
#[derive(Deserialize)]
pub struct RunRequest {
    pub pc_id: String,
    /// `"powershell"` (or `ps` / `pwsh`) or `"cmd"`. Default
    /// powershell.
    #[serde(default = "default_shell_str")]
    pub shell: String,
    pub script: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Optional. When set, `POST /api/jobs/<job_id>/kill` (or the
    /// CLI's `kanade kill`) can terminate this run.
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub jitter_secs: Option<u64>,
}

fn default_shell_str() -> String {
    "powershell".to_string()
}
fn default_timeout_secs() -> u64 {
    DEFAULT_RUN_TIMEOUT_SECS
}

pub async fn run(
    State(state): State<AppState>,
    Json(req): Json<RunRequest>,
) -> Result<Json<ExecResult>, (StatusCode, String)> {
    let shell = match req.shell.as_str() {
        "powershell" | "ps" | "pwsh" => Shell::Powershell,
        "cmd" => Shell::Cmd,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown shell {other:?} (use powershell or cmd)"),
            ));
        }
    };

    let request_id = Uuid::new_v4().to_string();
    let cmd = Command {
        id: "adhoc-run".to_string(),
        version: "0.0.0".to_string(),
        request_id: request_id.clone(),
        job_id: req.job_id.clone(),
        shell,
        script: req.script,
        timeout_secs: req.timeout_secs,
        jitter_secs: req.jitter_secs,
        // `kanade run` is inherently inline / one-PC / synchronous,
        // so the inherited agent identity (= LocalSystem in prod) is
        // always the right default. Use a Job + `kanade exec` if you
        // need run_as: user / system_gui.
        run_as: kanade_shared::wire::RunAs::System,
        // Same rationale: cwd customisation belongs on a registered
        // Job, not on inline ad-hoc runs.
        cwd: None,
        // Ad-hoc `kanade run` has no scheduled tick → no deadline.
        deadline_at: None,
        // v0.26: ad-hoc inline run has no Manifest, so there's no
        // operator-declared staleness policy to honour. Default to
        // `Cached` — matching pre-v0.26 behaviour where Layer 2 was
        // best-effort (silently pass when KV unreachable).
        staleness: kanade_shared::wire::Staleness::Cached,
    };

    let result_subj = subject::results(&request_id);
    let mut sub = state
        .nats
        .subscribe(result_subj.clone())
        .await
        .map_err(|e| {
            warn!(error = %e, request_id, "subscribe results subject");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("subscribe results: {e}"),
            )
        })?;
    // flush so the SUB is registered before we publish (see
    // reference_async_nats_subscribe_race).
    let _ = state.nats.flush().await;

    let payload = serde_json::to_vec(&cmd).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode Command: {e}"),
        )
    })?;
    state
        .nats
        .publish(subject::commands_pc(&req.pc_id), payload.into())
        .await
        .map_err(|e| {
            warn!(error = %e, pc_id = req.pc_id, "publish commands.pc.<id>");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("publish to {}: {e}", req.pc_id),
            )
        })?;
    let _ = state.nats.flush().await;

    info!(
        pc_id = %req.pc_id,
        request_id = %request_id,
        job_id = ?req.job_id,
        timeout_secs = req.timeout_secs,
        "sent command, waiting for result",
    );

    let wait = Duration::from_secs(req.timeout_secs + RESULT_WAIT_PADDING_SECS);
    let msg = tokio::time::timeout(wait, sub.next())
        .await
        .map_err(|_| {
            (
                StatusCode::REQUEST_TIMEOUT,
                format!("timeout waiting for result on {result_subj}"),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "result subscription closed".to_string(),
            )
        })?;

    let result: ExecResult = serde_json::from_slice(&msg.payload).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("decode ExecResult: {e}"),
        )
    })?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct PingQuery {
    /// Seconds to wait for one heartbeat. Default 45 (the agent's
    /// default heartbeat cadence is 30s, so 45s comfortably catches
    /// the next tick even with jitter).
    #[serde(default = "default_ping_wait")]
    pub wait_secs: u64,
}

fn default_ping_wait() -> u64 {
    DEFAULT_PING_WAIT_SECS
}

#[derive(Serialize)]
pub struct PingResponse {
    pub heartbeat: Heartbeat,
}

pub async fn ping(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
    Query(q): Query<PingQuery>,
) -> Result<Json<PingResponse>, (StatusCode, String)> {
    let subj = subject::heartbeat(&pc_id);
    let mut sub = state.nats.subscribe(subj.clone()).await.map_err(|e| {
        warn!(error = %e, pc_id, "subscribe heartbeat");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("subscribe {subj}: {e}"),
        )
    })?;
    let _ = state.nats.flush().await;

    info!(pc_id = %pc_id, wait_secs = q.wait_secs, "ping: waiting for heartbeat");
    let msg = tokio::time::timeout(Duration::from_secs(q.wait_secs), sub.next())
        .await
        .map_err(|_| {
            (
                StatusCode::REQUEST_TIMEOUT,
                format!("no heartbeat from {pc_id} within {}s", q.wait_secs),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "heartbeat subscription closed".to_string(),
            )
        })?;
    let hb: Heartbeat = serde_json::from_slice(&msg.payload).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("decode Heartbeat: {e}"),
        )
    })?;
    Ok(Json(PingResponse { heartbeat: hb }))
}
