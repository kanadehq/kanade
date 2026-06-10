//! Tauri 2.x app wiring for the Kanade Client.
//!
//! - On startup, connect to the agent's Named Pipe + run the
//!   SPEC §2.12.6 handshake (`KlpClient::connect`). The result
//!   is stashed in a Tauri-managed `AppState` so the `invoke`
//!   commands can read it from any window without reaching into
//!   globals.
//! - Commands today: `get_handshake` (returns the cached
//!   [`HandshakeResult`]), `ping_agent` (`system.ping`),
//!   `state_snapshot` (`state.snapshot` for the Health tab), and the
//!   `jobs_*` trio (`jobs.list` / `jobs.execute` / `jobs.kill`, #291).
//!   Each follow-up handler PR adds a sibling command and the matching
//!   WebView call.
//! - Push notifications: once connected, a forwarder task drains the
//!   client's notification broadcast (`jobs.progress`, `state.changed`,
//!   …) and re-emits each one to the WebView as a `klp-notification`
//!   Tauri event, so the UI updates a running job's progress without
//!   polling.
//!
//! Connection failure on startup is handled gracefully:
//! `AppState::klp` is an `Arc<Mutex<Option<KlpClient>>>`, so the
//! UI can render a "waiting for agent" banner and the user can
//! retry from the WebView once the agent service finishes
//! starting.

use std::sync::Arc;

use kanade_shared::ipc::handshake::HandshakeResult;
use kanade_shared::ipc::jobs::{
    JobCategory, JobsExecuteResult, JobsKillResult, JobsListParams, JobsListResult,
};
use kanade_shared::ipc::state::StateSnapshot;
use kanade_shared::ipc::system::PingResult;
use tauri::{Emitter, State};
use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::klp_client::KlpClient;

/// Tauri event name the WebView listens on for agent→client pushes.
/// Payload is the raw `RpcNotification` (`method` + `params`); the
/// WebView switches on `method` (`jobs.progress`, `state.changed`, …).
const NOTIFICATION_EVENT: &str = "klp-notification";

/// Emitted to the WebView each time a KLP connection is (re)established
/// (#468) so it can clear any "agent unavailable" banner and re-pull
/// its state. No payload.
const CONNECTED_EVENT: &str = "klp-connected";

/// Emitted when the live connection drops (agent restart / crash) before
/// the supervisor reconnects (#468), so the WebView can show a
/// "reconnecting…" banner instead of silently-failing commands.
const DISCONNECTED_EVENT: &str = "klp-disconnected";

/// Tauri-managed shared state. `Arc<Mutex<…>>` instead of plain
/// `Mutex<…>` so the spawned setup task can hold its own clone
/// while the `invoke` commands hold theirs.
pub struct AppState {
    klp: Arc<Mutex<Option<KlpClient>>>,
}

/// Clone the connected client out of the state lock, erroring if the
/// agent isn't connected yet. Done before every round-trip so the
/// `AppState` mutex isn't held across the await.
async fn connected_client(state: &State<'_, AppState>) -> Result<KlpClient, String> {
    state
        .klp
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "agent not connected".to_string())
}

#[tauri::command]
async fn get_handshake(state: State<'_, AppState>) -> Result<HandshakeResult, String> {
    let guard = state.klp.lock().await;
    match guard.as_ref() {
        Some(client) => Ok((*client.handshake()).clone()),
        None => Err("agent not connected (pipe unavailable on startup)".into()),
    }
}

#[tauri::command]
async fn ping_agent(state: State<'_, AppState>) -> Result<PingResult, String> {
    let client = connected_client(&state).await?;
    client.ping().await.map_err(|e| e.to_string())
}

/// `state.snapshot` — the endpoint health bundle the WebView's Health
/// tab renders (#290).
#[tauri::command]
async fn state_snapshot(state: State<'_, AppState>) -> Result<StateSnapshot, String> {
    let client = connected_client(&state).await?;
    client.snapshot().await.map_err(|e| e.to_string())
}

/// `jobs.list` — the user-invokable job catalog (#291). `category`
/// narrows to one tab (`software_update` / `troubleshoot` / `catalog`);
/// `None` returns every tab's jobs.
#[tauri::command]
async fn jobs_list(
    state: State<'_, AppState>,
    category: Option<JobCategory>,
) -> Result<JobsListResult, String> {
    let client = connected_client(&state).await?;
    client
        .jobs_list(&JobsListParams { category })
        .await
        .map_err(|e| e.to_string())
}

/// `jobs.execute` — run a user-invokable job by id (#291). Returns the
/// `run_id`; the run's `jobs.progress` arrives via the
/// `klp-notification` event stream, not this response.
#[tauri::command]
async fn jobs_execute(state: State<'_, AppState>, id: String) -> Result<JobsExecuteResult, String> {
    let client = connected_client(&state).await?;
    client.jobs_execute(&id).await.map_err(|e| e.to_string())
}

/// `jobs.kill` — request termination of a run this connection started
/// (#291).
#[tauri::command]
async fn jobs_kill(state: State<'_, AppState>, run_id: String) -> Result<JobsKillResult, String> {
    let client = connected_client(&state).await?;
    client.jobs_kill(&run_id).await.map_err(|e| e.to_string())
}

/// Drain the client's push-notification broadcast and re-emit each
/// notification to the WebView as a [`NOTIFICATION_EVENT`] Tauri event.
/// Runs until the connection closes (the broadcast sender drops). A
/// lagged subscriber (WebView fell behind) skips the dropped span and
/// keeps going — progress UX, not a transactional stream.
fn spawn_notification_forwarder(client: &KlpClient, handle: tauri::AppHandle) {
    let mut rx = client.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(notif) => {
                    if let Err(e) = handle.emit(NOTIFICATION_EVENT, notif) {
                        warn!(error = %e, "klp notification emit failed");
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "klp notification forwarder lagged; dropped pushes");
                }
                Err(RecvError::Closed) => {
                    info!("klp notification forwarder: connection closed, exiting");
                    return;
                }
            }
        }
    });
}

/// Keep a live KLP connection in `slot`, reconnecting whenever the
/// agent's pipe goes away (#468 — the agent self-updates, so service
/// restarts are routine). Loops forever:
///   connect (retrying while the agent is down) → publish the client +
///   a `klp-connected` event → forward notifications → block on
///   `wait_closed` → on close, clear the slot + emit `klp-disconnected`
///   → reconnect.
async fn supervise_connection(slot: Arc<Mutex<Option<KlpClient>>>, handle: tauri::AppHandle) {
    loop {
        // Connect, retrying — the user may launch the client before the
        // agent service is up, or it may be mid-restart. A one-shot
        // attempt would leave the slot `None` forever (the WebView's
        // `get_handshake` retry only reads the cache).
        let client = loop {
            match KlpClient::connect().await {
                Ok(c) => break c,
                Err(e) => {
                    warn!(error = %e, "KLP connect failed; retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        };
        info!(
            agent_version = %client.handshake().agent_version,
            "KLP client ready",
        );
        // Subscribe BEFORE publishing the client so no push between
        // connect and store is lost.
        spawn_notification_forwarder(&client, handle.clone());
        *slot.lock().await = Some(client.clone());
        if let Err(e) = handle.emit(CONNECTED_EVENT, ()) {
            warn!(error = %e, "klp-connected emit failed");
        }

        // Block until the reader task exits — the agent's pipe went
        // away (service restart / crash).
        client.wait_closed().await;
        warn!("KLP connection lost; reconnecting");
        *slot.lock().await = None;
        if let Err(e) = handle.emit(DISCONNECTED_EVENT, ()) {
            warn!(error = %e, "klp-disconnected emit failed");
        }
        // Loop back → reconnect.
    }
}

pub fn run() {
    let state = AppState {
        klp: Arc::new(Mutex::new(None)),
    };
    let klp_slot = state.klp.clone();

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_handshake,
            ping_agent,
            state_snapshot,
            jobs_list,
            jobs_execute,
            jobs_kill
        ])
        .setup(move |app| {
            // Supervise the KLP connection for the app's lifetime:
            // connect, and reconnect whenever the agent's pipe drops
            // (the agent self-updates, so restarts are routine) — #468.
            tauri::async_runtime::spawn(supervise_connection(
                klp_slot.clone(),
                app.handle().clone(),
            ));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kanade-client tauri application");
}
