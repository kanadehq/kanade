//! Tauri 2.x app wiring for the Kanade Client.
//!
//! - On startup, connect to the agent's Named Pipe + run the
//!   SPEC §2.12.6 handshake (`KlpClient::connect`). The result
//!   is stashed in a Tauri-managed `AppState` so the `invoke`
//!   commands can read it from any window without reaching into
//!   globals.
//! - Commands today: `get_handshake` (returns the cached
//!   [`HandshakeResult`]), `ping_agent` (`system.ping`),
//!   `state_snapshot` (`state.snapshot` for the Health tab), the
//!   `jobs_*` trio (`jobs.list` / `jobs.execute` / `jobs.kill`, #291),
//!   and the `notifications_*` trio (`notifications.subscribe` /
//!   `notifications.list` / `notifications.ack`, Phase E #102).
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
use kanade_shared::ipc::notifications::{
    NotificationsAckResult, NotificationsFilter, NotificationsListParams, NotificationsListResult,
    NotificationsSubscribeResult,
};
use kanade_shared::ipc::state::StateSnapshot;
use kanade_shared::ipc::system::PingResult;
use tauri::{Emitter, Manager, State};
use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::klp_client::KlpClient;

/// CLI flag the agent passes when it launches the client to surface an
/// emergency notification (Phase E, #102): `--show-notification <id>`.
/// Kept in sync with `kanade_agent::klp::emergency_notify`.
const SHOW_NOTIFICATION_FLAG: &str = "--show-notification";

/// The app's AppUserModelID — must match the tauri `identifier`, the
/// AUMID the notification plugin tags toasts with, AND the
/// `System.AppUserModel.ID` on the Start-Menu shortcut the installer
/// registers. All three aligned is what lets a non-MSIX desktop app's
/// WinRT toasts actually render (#102); a mismatch makes Windows silently
/// drop them. A compile-time `w!` PCWSTR — no runtime encode/alloc.
const APP_USER_MODEL_ID: windows::core::PCWSTR = windows::core::w!("com.yukimemi.kanade-client");

/// Same AUMID as [`APP_USER_MODEL_ID`], as a `&str` — `tauri-winrt-notification`
/// (the emergency-toast path) takes the app id as a string. Kept in lockstep
/// with the `w!` constant above and the tauri `identifier`.
const APP_USER_MODEL_ID_STR: &str = "com.yukimemi.kanade-client";

/// Pin this process's explicit AUMID to [`APP_USER_MODEL_ID`] at startup
/// so its toasts are associated with the registered shortcut. Best-effort
/// — a failure only means toasts may not render, not a crash.
fn set_app_user_model_id() {
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    // SAFETY: `APP_USER_MODEL_ID` is a compile-time NUL-terminated UTF-16
    // string; the API only reads it.
    if let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(APP_USER_MODEL_ID) } {
        warn!(error = ?e, "SetCurrentProcessExplicitAppUserModelID failed; toasts may not render");
    }
}

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

/// Emitted when a *second* process is launched with
/// `--show-notification <id>` while this instance is already running
/// (the single-instance guard, #624). Payload is the notification id;
/// the WebView toasts that emergency from the already-running instance
/// instead of a new process piling up.
const SHOW_NOTIFICATION_EVENT: &str = "klp-show-notification";

/// CLI flag the agent launches the client with to **re-surface** emergencies
/// the user couldn't see when they arrived — sent while signed out, or
/// delivered to the Action Center while the screen was locked — now that they
/// are present again (logon / unlock, #647). Kept in sync with
/// `kanade_agent::klp::emergency_notify`'s `RESURFACE_ARG`. A *fresh* launch
/// with this flag starts hidden and the WebView's `notifications.list`
/// recovery toasts every unread emergency; a *second* launch forwards it to
/// the running instance, which re-pops via [`RESURFACE_EVENT`].
const RESURFACE_FLAG: &str = "--resurface";

/// Emitted to the WebView when a *second* `--resurface` launch is collapsed
/// into the already-running instance (#647). No payload; the WebView re-toasts
/// every still-unread, unexpired emergency, deliberately bypassing its
/// duplicate-suppression (the user couldn't see them the first time).
const RESURFACE_EVENT: &str = "klp-resurface";

/// Tauri-managed shared state. `Arc<Mutex<…>>` instead of plain
/// `Mutex<…>` so the spawned setup task can hold its own clone
/// while the `invoke` commands hold theirs.
pub struct AppState {
    klp: Arc<Mutex<Option<KlpClient>>>,
    /// `Some(id)` when the app was launched by the agent's emergency
    /// fallback (`--show-notification <id>`): the WebView reads it on
    /// load to toast that notification and start hidden, instead of the
    /// normal visible startup. `None` on a normal user launch.
    launch_notification: Option<String>,
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

/// `notifications.subscribe` — start `notifications.new` pushes on the
/// connection (Phase E, #102). The WebView calls this on connect; the
/// pushes then arrive on the same `klp-notification` event stream as
/// `jobs.progress`, demuxed by `method` in the WebView.
#[tauri::command]
async fn notifications_subscribe(
    state: State<'_, AppState>,
) -> Result<NotificationsSubscribeResult, String> {
    let client = connected_client(&state).await?;
    client
        .notifications_subscribe()
        .await
        .map_err(|e| e.to_string())
}

/// `notifications.list` — the user's notification history (#102).
/// `filter` selects unread-only (default) vs all; `cursor` pages.
#[tauri::command]
async fn notifications_list(
    state: State<'_, AppState>,
    filter: Option<NotificationsFilter>,
    cursor: Option<String>,
) -> Result<NotificationsListResult, String> {
    let client = connected_client(&state).await?;
    let params = NotificationsListParams {
        filter: filter.unwrap_or_default(),
        cursor,
        ..NotificationsListParams::default()
    };
    client
        .notifications_list(&params)
        .await
        .map_err(|e| e.to_string())
}

/// `notifications.ack` — mark a notification read for the caller's OS
/// user (#102). The `run_id`-less analogue of the remediation flow: the
/// agent persists the ack + emits the `events.notifications.acked.>`
/// event the SPA's confirmation view consumes.
#[tauri::command]
async fn notifications_ack(
    state: State<'_, AppState>,
    id: String,
) -> Result<NotificationsAckResult, String> {
    let client = connected_client(&state).await?;
    client
        .notifications_ack(&id)
        .await
        .map_err(|e| e.to_string())
}

/// The notification id this app was launched to surface, or `None` on a
/// normal launch (Phase E emergency fallback, #102). The WebView calls
/// this on load: a `Some(id)` means "show a toast for this emergency and
/// stay hidden", a `None` means "normal visible startup".
#[tauri::command]
fn get_launch_notification(state: State<'_, AppState>) -> Option<String> {
    state.launch_notification.clone()
}

/// Reveal + focus the main window. Called from the WebView when the user
/// clicks the emergency toast (the window was started hidden so the toast
/// never bursts over a meeting); also used to bring an already-running
/// client forward. Best-effort — a missing window just logs.
#[tauri::command]
fn show_main_window(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    } else {
        warn!("show_main_window: no 'main' window");
    }
}

/// Show an emergency notification as a NATIVE WinRT toast (#102).
///
/// The `@tauri-apps/plugin-notification` `sendNotification` can't make a toast
/// **persist** on desktop, so the emergency path bypasses it and builds the
/// toast directly here with `Scenario::Reminder` (+ an action button, which
/// the scenario needs to actually stay on screen) — it stays until the user
/// dismisses it and lands in the Action Center, instead of the plugin's ~7 s
/// auto-dismiss (an emergency that vanishes in seconds is useless).
///
/// No `on_activated` (toast-click) handler: registering
/// `ToastNotification.Activated` requires a non-MSIX app to have a
/// COM-activator CLSID registered (registry + the shortcut's
/// `ToastActivatorCLSID`); without it the `Activated` call inside `show()`
/// fails and the toast never submits. Reveal-on-click is deferred to #647.
///
/// The toast is tagged with [`APP_USER_MODEL_ID_STR`] (the same AUMID the
/// process pins + the Start-Menu shortcut carries), which is what lets a
/// non-MSIX desktop app's toast render at all. Returns `Err` if the toast
/// can't be shown so the WebView can fall back to the plugin path.
#[tauri::command]
async fn show_emergency_toast(title: String, body: String) -> Result<(), String> {
    info!(title = %title, "show_emergency_toast: invoked");
    // Build + show the toast on the async runtime — the SAME context the
    // notification plugin submits from. Showing it on the command worker
    // thread or hopping to the main thread via `run_on_main_thread` both
    // returned without the toast ever reaching the platform. AWAIT the task so
    // a `show()` failure is returned to the WebView (which then falls back to
    // the plugin toast) rather than only logged.
    let handle = tauri::async_runtime::spawn(async move {
        use tauri_winrt_notification::{Scenario, Toast};

        Toast::new(APP_USER_MODEL_ID_STR)
            .title(&title)
            .text1(&body)
            // Pre-expanded, stays on screen until dismissed and persists in
            // the Action Center — the whole point of an emergency (vs the
            // default ~7 s auto-dismiss).
            .scenario(Scenario::Reminder)
            // `scenario=Reminder` only actually persists ("stays until the
            // user dismisses it") when the toast carries at least one action
            // — without a button Windows treats it as an ordinary toast and
            // auto-dismisses it in seconds (confirmed live). This button
            // satisfies that requirement; wiring its click to ack/reveal the
            // app needs a COM activator (issue #647), so for now it only
            // guarantees persistence.
            .add_button("確認", "ack")
            .show()
            .map_err(|e| e.to_string())
    });
    match handle.await {
        Ok(Ok(())) => {
            info!("show_emergency_toast: toast.show() ok");
            Ok(())
        }
        Ok(Err(e)) => {
            warn!(error = %e, "show_emergency_toast: native toast failed");
            Err(e)
        }
        Err(e) => {
            warn!(error = %e, "show_emergency_toast: toast task did not complete");
            Err(format!("toast task did not complete: {e}"))
        }
    }
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

/// Parse `--show-notification <id>` out of an arbitrary arg sequence.
/// Returns the id when the flag is present with a non-empty value.
/// Shared by the startup parse (`std::env::args`) and the single-instance
/// handler (which gets the *second* process's argv, #624).
fn parse_show_notification<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == SHOW_NOTIFICATION_FLAG {
            return it.next().filter(|s| !s.is_empty());
        }
    }
    None
}

/// Parse `--show-notification <id>` from this process's own args (Phase E
/// emergency fallback). Returns the id when present and non-empty.
fn parse_launch_notification() -> Option<String> {
    parse_show_notification(std::env::args())
}

/// Whether `--resurface` is present in an arg sequence (#647). Shared by the
/// startup parse (`std::env::args`) and the single-instance handler.
fn has_resurface_flag<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == RESURFACE_FLAG)
}

/// Single-instance callback (#624): fires in the ALREADY-RUNNING instance
/// when a second `kanade-client` is launched (the agent's emergency
/// fallback launches one per emergency, and without this guard each would
/// pile up a new hidden process). The second process forwards its argv
/// here and exits.
///
/// - Launched for an emergency (`--show-notification <id>`): forward the
///   id to the WebView so the running instance toasts that emergency —
///   the window stays hidden (clicking the toast reveals it), matching the
///   normal emergency-launch UX.
/// - Any other second launch (e.g. the user double-clicks the exe): just
///   surface the existing window.
#[cfg(target_os = "windows")]
fn on_second_instance(app: &tauri::AppHandle, argv: Vec<String>) {
    if has_resurface_flag(argv.iter().cloned()) {
        // Presence-driven re-surface (#647): the already-running instance
        // re-pops every unread emergency; the window stays hidden.
        if let Err(e) = app.emit(RESURFACE_EVENT, ()) {
            warn!(error = %e, "single-instance: forward resurface failed");
        }
    } else if let Some(id) = parse_show_notification(argv) {
        if let Err(e) = app.emit(SHOW_NOTIFICATION_EVENT, id) {
            warn!(error = %e, "single-instance: forward emergency id failed");
        }
    } else if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    } else {
        // Match `show_main_window`'s contract — surface the miss in traces
        // rather than a silent no-op (Claude #643).
        warn!("single-instance: plain second launch — 'main' window not found");
    }
}

pub fn run() {
    // Pin the process AUMID before anything else so toasts render (#102).
    set_app_user_model_id();
    let launch_notification = parse_launch_notification();
    // Launched by the agent for an emergency (`--show-notification <id>`) or a
    // presence-driven re-surface (`--resurface`, #647) → start hidden (the
    // WebView toasts; the window only appears when the user clicks the toast),
    // so it never bursts over whatever the user is doing.
    let launched_for_emergency =
        launch_notification.is_some() || has_resurface_flag(std::env::args());
    let state = AppState {
        klp: Arc::new(Mutex::new(None)),
        launch_notification,
    };
    let klp_slot = state.klp.clone();

    tauri::Builder::default()
        // Single-instance guard (#624) — MUST be the first plugin. A
        // second launch (the agent's per-emergency fallback, or a user
        // double-click) forwards its argv to this callback and exits, so
        // hidden client processes never pile up; the running instance
        // toasts the forwarded emergency instead.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            on_second_instance(app, argv);
        }))
        // Native OS toasts for the emergency fallback (shown from the
        // WebView via @tauri-apps/plugin-notification).
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_handshake,
            ping_agent,
            state_snapshot,
            jobs_list,
            jobs_execute,
            jobs_kill,
            notifications_subscribe,
            notifications_list,
            notifications_ack,
            get_launch_notification,
            show_main_window,
            show_emergency_toast
        ])
        .setup(move |app| {
            // Supervise the KLP connection for the app's lifetime:
            // connect, and reconnect whenever the agent's pipe drops
            // (the agent self-updates, so restarts are routine) — #468.
            tauri::async_runtime::spawn(supervise_connection(
                klp_slot.clone(),
                app.handle().clone(),
            ));
            // The window starts hidden (tauri.conf `visible: false`). On a
            // normal launch, reveal it immediately; on an emergency
            // launch, leave it hidden until the user clicks the toast
            // (`show_main_window`).
            if !launched_for_emergency {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                } else {
                    // "main" is always defined in tauri.conf, so this is
                    // theoretical — but match `show_main_window`'s contract
                    // and don't leave the window silently hidden.
                    warn!("setup: 'main' window not found on normal launch");
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kanade-client tauri application");
}
