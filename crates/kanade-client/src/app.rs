//! Tauri 2.x app wiring for the Kanade Client.
//!
//! - On startup, connect to the agent's Named Pipe + run the
//!   SPEC §2.12.6 handshake (`KlpClient::connect`). The result
//!   is stashed in a Tauri-managed `AppState` so the `invoke`
//!   commands can read it from any window without reaching into
//!   globals.
//! - Three commands today: `get_handshake` (returns the cached
//!   [`HandshakeResult`]), `ping_agent` (round-trips `system.ping`),
//!   and `state_snapshot` (round-trips `state.snapshot` for the
//!   Health tab). Each follow-up handler PR adds a sibling command
//!   and the matching WebView call.
//!
//! Connection failure on startup is handled gracefully:
//! `AppState::klp` is an `Arc<Mutex<Option<KlpClient>>>`, so the
//! UI can render a "waiting for agent" banner and the user can
//! retry from the WebView once the agent service finishes
//! starting.

use std::sync::Arc;

use kanade_shared::ipc::handshake::HandshakeResult;
use kanade_shared::ipc::state::StateSnapshot;
use kanade_shared::ipc::system::PingResult;
use tauri::State;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::klp_client::KlpClient;

/// Tauri-managed shared state. `Arc<Mutex<…>>` instead of plain
/// `Mutex<…>` so the spawned setup task can hold its own clone
/// while the `invoke` commands hold theirs.
pub struct AppState {
    klp: Arc<Mutex<Option<KlpClient>>>,
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
    let client = {
        let guard = state.klp.lock().await;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| "agent not connected".to_string())?
    };
    client.ping().await.map_err(|e| e.to_string())
}

/// `state.snapshot` — the endpoint health bundle the WebView's Health
/// tab renders (#290). Clone the client out of the lock first so the
/// round-trip doesn't hold the `AppState` mutex across the await.
#[tauri::command]
async fn state_snapshot(state: State<'_, AppState>) -> Result<StateSnapshot, String> {
    let client = {
        let guard = state.klp.lock().await;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| "agent not connected".to_string())?
    };
    client.snapshot().await.map_err(|e| e.to_string())
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
            state_snapshot
        ])
        .setup(move |_app| {
            // `app.handle()` will be needed here in the next PR
            // (emit a "klp-ready" event the WebView listens for
            // so it can re-render without the WebView's polling
            // loop). Today the setup retries the connect itself
            // — the user might launch the client before the
            // agent service has finished starting, and a one-shot
            // attempt would leave `AppState::klp` `None` forever
            // (the WebView's `get_handshake` retry only reads the
            // cache, it doesn't trigger a fresh connect).
            let slot = klp_slot.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match KlpClient::connect().await {
                        Ok(client) => {
                            info!(
                                agent_version = %client.handshake().agent_version,
                                "KLP client ready",
                            );
                            *slot.lock().await = Some(client);
                            return;
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                "KLP client connect failed; retrying in 5s",
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kanade-client tauri application");
}
