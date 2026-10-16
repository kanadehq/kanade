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
use kanade_shared::ipc::jobs::{JobsExecuteResult, JobsKillResult, JobsListParams, JobsListResult};
use kanade_shared::ipc::notifications::{
    NotificationsAckResult, NotificationsFilter, NotificationsListParams, NotificationsListResult,
    NotificationsSubscribeResult, NotificationsUnackResult,
};
use kanade_shared::ipc::state::StateSnapshot;
use kanade_shared::ipc::support::{SupportLockResult, SupportStatusResult, SupportUnlockResult};
use kanade_shared::ipc::system::PingResult;
use tauri::{Emitter, Manager, State};
use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::klp_client::KlpClient;
use kanade_client::startup::{
    ExistingInstanceAction, StartupIntent, has_resurface_flag, parse_protocol_show,
    parse_show_notification,
};
use kanade_client::window_size;

/// Built-in fallback product name the client shows when no operator
/// has configured `agent_config.client_display_name`. Aliases the
/// single Rust source of truth in `kanade-shared` so this and the
/// agent's Start-Menu default can't drift; the WebView's
/// `DEFAULT_DISPLAY_NAME` (web/src/main.ts) + index.html mirror the same
/// literal. The client UI is Japanese-only (no i18n), so the default is
/// Japanese; English fleets override it via the backend config.
const DEFAULT_DISPLAY_NAME: &str = kanade_shared::DEFAULT_CLIENT_DISPLAY_NAME;

/// The app's AppUserModelID — must match the tauri `identifier`, the
/// AUMID the notification plugin tags toasts with, AND the
/// `System.AppUserModel.ID` on the Start-Menu shortcut the installer
/// registers. All three aligned is what lets a non-MSIX desktop app's
/// WinRT toasts actually render (#102); a mismatch makes Windows silently
/// drop them. A compile-time `w!` PCWSTR — no runtime encode/alloc.
#[cfg(target_os = "windows")]
const APP_USER_MODEL_ID: windows::core::PCWSTR = windows::core::w!("com.yukimemi.kanade-client");

/// Same AUMID as [`APP_USER_MODEL_ID`], as a `&str` — `tauri-winrt-notification`
/// (the emergency-toast path) takes the app id as a string. Kept in lockstep
/// with the `w!` constant above and the tauri `identifier`.
#[cfg(target_os = "windows")]
const APP_USER_MODEL_ID_STR: &str = "com.yukimemi.kanade-client";

/// Pin this process's explicit AUMID to [`APP_USER_MODEL_ID`] at startup
/// so its toasts are associated with the registered shortcut. Best-effort
/// — a failure only means toasts may not render, not a crash.
#[cfg(target_os = "windows")]
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
#[cfg(target_os = "windows")]
const SHOW_NOTIFICATION_EVENT: &str = "klp-show-notification";

/// Emitted to the WebView when a *second* `--resurface` launch is collapsed
/// into the already-running instance (#647). No payload; the WebView re-toasts
/// every still-unread, unexpired emergency, deliberately bypassing its
/// duplicate-suppression (the user couldn't see them the first time).
#[cfg(target_os = "windows")]
const RESURFACE_EVENT: &str = "klp-resurface";

/// Emitted when a toast was **clicked** (#647): the user activated a
/// `kanade-client://show?id=<id>` toast (body or 確認 button), which the
/// single-instance guard forwarded here. Payload is the notification id; the
/// WebView reveals the window and scrolls to / flashes that notification.
#[cfg(target_os = "windows")]
const FOCUS_NOTIFICATION_EVENT: &str = "klp-focus-notification";

/// Tauri-managed shared state. `Arc<Mutex<…>>` instead of plain
/// `Mutex<…>` so the spawned setup task can hold its own clone
/// while the `invoke` commands hold theirs.
pub struct AppState {
    klp: Arc<Mutex<Option<KlpClient>>>,
    /// Retained so a WebView loaded after admission failure can read the reason.
    rejection: Arc<Mutex<Option<String>>>,
    /// `Some(id)` when the app was launched by the agent's emergency
    /// fallback (`--show-notification <id>`): the WebView reads it on
    /// load to toast that notification and start hidden, instead of the
    /// normal visible startup. `None` on a normal user launch.
    launch_notification: Option<String>,
    /// `Some(id)` when the app was launched by a **toast click**
    /// (`kanade-client://show?id=<id>` protocol, #647): the WebView reads it on
    /// load to scroll to + flash that notification — with the window *visible*
    /// (the user asked to see it), unlike `launch_notification`.
    launch_focus: Option<String>,
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

/// Permanent admission failure, including one emitted before the WebView loaded.
#[tauri::command]
async fn get_connection_rejection(state: State<'_, AppState>) -> Result<Option<String>, String> {
    Ok(state.rejection.lock().await.clone())
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

/// `jobs.list` — the user-invokable job catalog (#291). `category` is a
/// free-form key (#792) narrowing to one tab; `None` returns every tab's
/// jobs.
#[tauri::command]
async fn jobs_list(
    state: State<'_, AppState>,
    category: Option<String>,
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

/// `support.unlock` — redeem a support code typed by the IT desk, revealing
/// this user's `client.unlock`-gated jobs for the grant's lifetime. The
/// error string reaches the WebView verbatim; a wrong code is
/// `Unauthorized`, a rate-limited caller `RateLimit`.
#[tauri::command]
async fn support_unlock(
    state: State<'_, AppState>,
    code: String,
) -> Result<SupportUnlockResult, String> {
    let client = connected_client(&state).await?;
    client
        .support_unlock(&code)
        .await
        .map_err(|e| e.to_string())
}

/// `support.lock` — end the support session immediately.
#[tauri::command]
async fn support_lock(state: State<'_, AppState>) -> Result<SupportLockResult, String> {
    let client = connected_client(&state).await?;
    client.support_lock().await.map_err(|e| e.to_string())
}

/// `support.status` — the grants this user currently holds, so a WebView
/// that (re)connected can restore its support-mode banner without asking
/// the desk to re-type the code.
#[tauri::command]
async fn support_status(state: State<'_, AppState>) -> Result<SupportStatusResult, String> {
    let client = connected_client(&state).await?;
    client.support_status().await.map_err(|e| e.to_string())
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

/// `notifications.unack` — retract a confirmation the user made by
/// mistake (the read↔unread toggle). Inverse of [`notifications_ack`]:
/// the agent deletes the read mark + emits `events.notifications.unacked.>`
/// so the SPA roster reverts to "未確認" while keeping the audit trail.
#[tauri::command]
async fn notifications_unack(
    state: State<'_, AppState>,
    id: String,
) -> Result<NotificationsUnackResult, String> {
    let client = connected_client(&state).await?;
    client
        .notifications_unack(&id)
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

/// The notification id this app was launched to **focus** via a toast click
/// (`kanade-client://show?id=<id>`, #647), or `None`. The WebView calls this on
/// load: `Some(id)` means "the window is visible — scroll to + flash this
/// notification".
#[tauri::command]
fn get_launch_focus(state: State<'_, AppState>) -> Option<String> {
    state.launch_focus.clone()
}

/// This client binary's build version (the workspace `CARGO_PKG_VERSION`,
/// e.g. `0.43.63`). The WebView renders it dim in the sidebar footer so the
/// user can confirm what's actually running — mirrors the SPA's
/// `GET /api/version` badge. Baked at compile time, so it reflects the
/// *running* binary, not whatever's on disk (a fleet-deploy swaps the file
/// but the live process keeps its old version until restarted).
#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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

/// Open an `http(s)` URL in the user's default browser. Called from the
/// WebView when the user clicks a link auto-detected in a notification body
/// (the panel renders bare URLs as `<a class="notif-link">`). The WebView
/// can't just navigate an `<a href>` — that would replace the embedded app
/// page with the remote site — so link clicks are routed here to launch the
/// OS default handler instead.
///
/// Only `http://` / `https://` are honoured: a notification body is
/// operator-authored content rendered in a *privileged* WebView (it can
/// `invoke` agent commands), so we refuse `file:`, `javascript:`,
/// `kanade-client:` and every other scheme rather than hand an arbitrary URI
/// to `ShellExecuteW`. The WebView validates the scheme too; this is the
/// defence-in-depth backstop. Best-effort: a launch failure is logged, not
/// surfaced (the user can still copy the visible URL text).
///
/// `async` so Tauri runs it off the UI (main) thread: a synchronous command
/// runs on the main thread, and `ShellExecuteW` can block briefly (DDE
/// timeout, cold browser start) which would freeze the WebView for that window.
#[cfg(target_os = "windows")]
#[tauri::command]
async fn open_external_url(url: String) {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        warn!(%url, "open_external_url: refusing non-http(s) scheme");
        return;
    }
    open_windows_url(&url);
}

#[cfg(target_os = "macos")]
#[tauri::command]
async fn open_external_url(url: String) {
    if url.starts_with("https://") || url.starts_with("http://") {
        match std::process::Command::new("/usr/bin/open")
            .arg(&url)
            .status()
        {
            Ok(status) if status.success() => {}
            result => warn!(?result, "browser open failed"),
        }
    }
}

#[cfg(target_os = "windows")]
fn open_windows_url(url: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    // ShellExecuteW with the "open" verb hands the URL straight to the
    // registered protocol handler (the default browser) — no shell, so no
    // command-injection surface even before the scheme gate above.
    let url_h = HSTRING::from(url);
    // SAFETY: all pointers are valid for the call; we ignore the returned
    // pseudo-HINSTANCE (only meaningful as a >32 success sentinel here).
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &url_h,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute returns a value <= 32 on failure.
    if result.0 as isize <= 32 {
        warn!(%url, code = result.0 as isize, "open_external_url: ShellExecuteW failed");
    } else {
        info!(%url, "open_external_url: opened in default browser");
    }
}

/// Escape the five XML metacharacters so a title/body/URI is safe inside the
/// toast XML (both element text and double-quoted attributes).
#[cfg(target_os = "windows")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Show an emergency notification as a NATIVE WinRT toast (#102 / #647).
///
/// The `@tauri-apps/plugin-notification` `sendNotification` can neither make a
/// toast **persist** nor carry a click target on desktop, so the emergency
/// path builds the toast XML directly:
///
/// - `scenario="reminder"` (+ one action, required for the scenario to take) →
///   stays on screen until dismissed and persists in the Action Center,
///   instead of the plugin's ~7 s auto-dismiss.
/// - toast-level `launch="kanade-client://show?id=<id>" activationType="protocol"`
///   (and the same on the 確認 button) → a **body OR button click opens the
///   client** focused on this notification, via the registered
///   `kanade-client://` protocol. Protocol activation needs no COM activator
///   (that's only for in-process foreground/background activation), so this
///   sidesteps the registration the `Activated`-event path would require.
///
/// Tagged with [`APP_USER_MODEL_ID_STR`] (the AUMID the process pins + the
/// shortcut carries) so a non-MSIX desktop toast renders at all. Returns `Err`
/// (so the WebView can fall back to the plugin path) if the toast can't show.
#[cfg(target_os = "windows")]
#[tauri::command]
async fn show_emergency_toast(title: String, body: String, id: String) -> Result<(), String> {
    info!(title = %title, %id, "show_emergency_toast: invoked");
    // Build + show on the async runtime — the SAME context the notification
    // plugin submits from. A command worker thread / `run_on_main_thread` both
    // returned without the toast reaching the platform. AWAIT so a failure is
    // returned to the WebView (which then falls back to the plugin toast).
    let handle = tauri::async_runtime::spawn(async move {
        use windows::Data::Xml::Dom::XmlDocument;
        use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
        use windows::core::HSTRING;

        // Build the URI in its natural form, then XML-escape once at the
        // embedding point (the conventional pattern). The id charset is
        // `[A-Za-z0-9_.-]` so today `xml_escape` is a no-op here, but escaping
        // the whole URI keeps it correct-by-construction: escaping `id` first
        // and embedding the result would double-encode an `&` (XML `&amp;` →
        // the protocol handler receives `&`, truncating the id).
        let uri = format!("kanade-client://show?id={id}");
        let uri_xml = xml_escape(&uri);
        let xml = format!(
            "<toast launch=\"{uri_xml}\" activationType=\"protocol\" scenario=\"reminder\">\
               <visual><binding template=\"ToastGeneric\">\
                 <text>{title}</text><text>{body}</text>\
               </binding></visual>\
               <actions>\
                 <action content=\"確認\" activationType=\"protocol\" arguments=\"{uri_xml}\"/>\
               </actions>\
             </toast>",
            uri_xml = uri_xml,
            title = xml_escape(&title),
            body = xml_escape(&body),
        );

        let doc = XmlDocument::new().map_err(|e| e.to_string())?;
        doc.LoadXml(&HSTRING::from(&xml))
            .map_err(|e| e.to_string())?;
        let toast = ToastNotification::CreateToastNotification(&doc).map_err(|e| e.to_string())?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
            APP_USER_MODEL_ID_STR,
        ))
        .map_err(|e| e.to_string())?;
        notifier.Show(&toast).map_err(|e| e.to_string())
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

#[cfg(target_os = "macos")]
#[tauri::command]
async fn show_emergency_toast(title: String, body: String, id: String) -> Result<(), String> {
    let _ = (title, body, id);
    Err("Native notifications are unavailable in this macOS stage; use history".into())
}

/// The missing macOS session adapter supplies no verified identity or lock facts.
/// Manual UI/history commands never consult automatic delivery eligibility.
#[tauri::command]
fn automatic_notifications_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        true
    }
    #[cfg(target_os = "macos")]
    {
        use kanade_client::session::{
            AutomaticDelivery, GuiSessionFacts, GuiTarget, automatic_delivery,
        };
        let facts = GuiSessionFacts::default();
        let target = GuiTarget {
            uid: unsafe { libc::geteuid() },
            session_id: String::new(),
        };
        matches!(
            automatic_delivery(&target, &facts),
            AutomaticDelivery::Eligible
        )
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
/// restarts are routine). Stops only on permanent admission rejection:
///   connect (retrying while the agent is down) → publish the client +
///   a `klp-connected` event → forward notifications → block on
///   `wait_closed` → on close, clear the slot + emit `klp-disconnected`
///   → reconnect.
async fn supervise_connection(
    slot: Arc<Mutex<Option<KlpClient>>>,
    rejection: Arc<Mutex<Option<String>>>,
    handle: tauri::AppHandle,
) {
    loop {
        // Connect, retrying — the user may launch the client before the
        // agent service is up, or it may be mid-restart. A one-shot
        // attempt would leave the slot `None` forever (the WebView's
        // `get_handshake` retry only reads the cache).
        let client = match KlpClient::connect_with_retry().await {
            Ok(client) => client,
            Err(error) => {
                let reason = error.to_string();
                *rejection.lock().await = Some(reason.clone());
                warn!(%error, "KLP authorization rejected; reconnect stopped");
                if let Err(e) = handle.emit("klp-rejected", reason) {
                    warn!(error = %e, "klp-rejected emit failed");
                }
                return;
            }
        };
        info!(
            agent_version = %client.handshake().agent_version,
            "KLP client ready",
        );
        // Brand the OS window title from the operator-configured
        // product name (handshake `client_display_name`), falling back
        // to the built-in default. This is the title Windows shows in
        // the title bar + Alt-Tab; the WebView separately swaps its
        // in-page header from the same field via `get_handshake`. Set
        // it on every (re)connect so a config change picked up after an
        // agent restart re-brands the live window without relaunch.
        {
            let title = client
                .handshake()
                .client_display_name
                .clone()
                .unwrap_or_else(|| DEFAULT_DISPLAY_NAME.to_string());
            if let Some(win) = handle.get_webview_window("main") {
                if let Err(e) = win.set_title(&title) {
                    warn!(error = %e, "set window title from client_display_name failed");
                }
            }
        }
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

fn parse_launch_notification() -> Option<String> {
    parse_show_notification(std::env::args())
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
    let notification = parse_show_notification(argv.iter().cloned());
    let focus = parse_protocol_show(&argv);
    let intent = StartupIntent {
        resurface: has_resurface_flag(argv.iter().cloned()),
        notification: notification.is_some(),
        deep_link: focus.is_some(),
        // Preserve Windows argv behavior; Unix adapters will set --tray.
        tray: false,
    };
    if intent.existing_instance_action() == ExistingInstanceAction::Resurface {
        // Presence-driven re-surface (#647): the already-running instance
        // re-pops every unread emergency; the window stays hidden.
        if let Err(e) = app.emit(RESURFACE_EVENT, ()) {
            warn!(error = %e, "single-instance: forward resurface failed");
        }
    } else if let Some(id) = focus {
        // Toast clicked (#647 `kanade-client://show?id=<id>` protocol launch):
        // reveal the window and tell the WebView to focus this notification.
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
        }
        if let Err(e) = app.emit(FOCUS_NOTIFICATION_EVENT, id) {
            warn!(error = %e, "single-instance: forward focus id failed");
        }
    } else if let Some(id) = notification {
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

#[cfg(target_os = "macos")]
fn on_second_instance(app: &tauri::AppHandle, argv: Vec<String>) {
    // Full URL-open/focus queues and session-scoped instance identity are later stages.
    if StartupIntent::from_args(&argv).existing_instance_action() == ExistingInstanceAction::Show {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}

/// Debounce generation for resize-driven saves; a newer resize supersedes
/// any pending save so a stale task never overwrites a newer size.
static RESIZE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

#[cfg(target_os = "windows")]
fn window_size_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|d| d.join(window_size::FILE_NAME))
}

/// Configured (minWidth, minHeight) of the main window, in logical px.
fn main_window_min(app: &tauri::AppHandle) -> (f64, f64) {
    let (dw, dh) = window_size::DEFAULT_MIN;
    app.config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .map(|w| (w.min_width.unwrap_or(dw), w.min_height.unwrap_or(dh)))
        .unwrap_or((dw, dh))
}

/// Largest client-area size (logical px) that fits the window's monitor work
/// area, minus the non-client frame. `None` when no monitor is known.
fn max_client_size(win: &tauri::Window) -> Option<(f64, f64)> {
    let mon = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| win.primary_monitor().ok().flatten())?;
    let scale = mon.scale_factor();
    if !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    let wa = mon.work_area().size;
    let (mut fw, mut fh) = (0.0, 0.0);
    if let (Ok(o), Ok(i)) = (win.outer_size(), win.inner_size()) {
        fw = f64::from(o.width.saturating_sub(i.width));
        fh = f64::from(o.height.saturating_sub(i.height));
    }
    Some((
        (f64::from(wa.width) - fw) / scale,
        (f64::from(wa.height) - fh) / scale,
    ))
}

/// Save the current client size. Skipped while minimized / maximized /
/// fullscreen (or when that state can't be read). Failures are only logged.
pub(crate) fn persist_window_size(win: &tauri::Window) {
    if win.label() != "main" {
        return;
    }
    let normal = matches!(
        (win.is_minimized(), win.is_maximized(), win.is_fullscreen()),
        (Ok(false), Ok(false), Ok(false))
    );
    if !normal {
        return;
    }
    let (Ok(inner), Ok(scale)) = (win.inner_size(), win.scale_factor()) else {
        return;
    };
    if !(scale.is_finite() && scale > 0.0) {
        return;
    }
    let app = win.app_handle();
    #[cfg(target_os = "windows")]
    if let Some(path) = window_size_path(app) {
        window_size::save_best_effort(
            &path,
            f64::from(inner.width) / scale,
            f64::from(inner.height) / scale,
            main_window_min(app),
            max_client_size(win),
        );
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = kanade_client::macos_storage::save_size(
        f64::from(inner.width) / scale,
        f64::from(inner.height) / scale,
        main_window_min(app),
        max_client_size(win),
    ) {
        warn!(%error, "window size: save failed");
    }
}

fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    use std::sync::atomic::Ordering;
    if window.label() != "main" {
        return;
    }
    match event {
        tauri::WindowEvent::Resized(_) => {
            let id = RESIZE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
            let win = window.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(RESIZE_DEBOUNCE).await;
                if RESIZE_GEN.load(Ordering::SeqCst) == id {
                    persist_window_size(&win);
                }
            });
        }
        // Flush immediately; the debounce task may never get to run.
        tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed => {
            persist_window_size(window);
        }
        _ => {}
    }
}

/// Apply the saved size before the window is first shown. Never fails startup.
fn restore_window_size(app: &tauri::AppHandle) {
    let Some(webview_window) = app.get_webview_window("main") else {
        return;
    };
    let win = AsRef::<tauri::Webview>::as_ref(&webview_window).window();
    #[cfg(target_os = "windows")]
    let size = window_size_path(app)
        .and_then(|path| window_size::load(&path, main_window_min(app), max_client_size(&win)));
    #[cfg(target_os = "macos")]
    let size = kanade_client::macos_storage::load_size(main_window_min(app), max_client_size(&win))
        .unwrap_or_else(|error| {
            warn!(%error, "window size: restore failed");
            None
        });
    let Some(size) = size else {
        return;
    };
    let _ = win.set_size(tauri::LogicalSize::new(
        f64::from(size.width),
        f64::from(size.height),
    ));
}

pub fn run() {
    // Pin the process AUMID before anything else so toasts render (#102).
    #[cfg(target_os = "windows")]
    set_app_user_model_id();
    let launch_notification = parse_launch_notification();
    // Launched by a toast click (`kanade-client://show?id=<id>`, #647) → the
    // user asked to SEE it, so the window is visible and the WebView focuses
    // the notification (vs `launch_notification`, which starts hidden + toasts).
    let launch_focus = parse_protocol_show(std::env::args());
    // Launched by the agent for an emergency (`--show-notification <id>`) or a
    // presence-driven re-surface (`--resurface`, #647) → start hidden (the
    // WebView toasts; the window only appears when the user clicks the toast),
    // so it never bursts over whatever the user is doing. A toast-click launch
    // (`launch_focus`) is NOT hidden.
    let initially_visible = StartupIntent {
        notification: launch_notification.is_some(),
        resurface: has_resurface_flag(std::env::args()),
        deep_link: launch_focus.is_some(),
        // Windows tray handling is unchanged in this stage.
        tray: cfg!(target_os = "macos") && std::env::args().any(|arg| arg == "--tray"),
    }
    .initially_visible();
    let state = AppState {
        klp: Arc::new(Mutex::new(None)),
        rejection: Arc::new(Mutex::new(None)),
        launch_notification,
        launch_focus,
    };
    let klp_slot = state.klp.clone();
    let rejection = state.rejection.clone();

    let builder = tauri::Builder::default()
        // Single-instance guard (#624) — MUST be the first plugin. A
        // second launch (the agent's per-emergency fallback, or a user
        // double-click) forwards its argv to this callback and exits, so
        // hidden client processes never pile up; the running instance
        // toasts the forwarded emergency instead.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            on_second_instance(app, argv);
        }));
    #[cfg(target_os = "windows")]
    let builder = builder.plugin(tauri_plugin_notification::init());
    builder
        .manage(state)
        .on_window_event(on_window_event)
        .invoke_handler(tauri::generate_handler![
            automatic_notifications_available,
            get_handshake,
            get_connection_rejection,
            ping_agent,
            state_snapshot,
            jobs_list,
            jobs_execute,
            jobs_kill,
            support_unlock,
            support_lock,
            support_status,
            notifications_subscribe,
            notifications_list,
            notifications_ack,
            notifications_unack,
            get_launch_notification,
            get_launch_focus,
            app_version,
            show_main_window,
            show_emergency_toast,
            open_external_url
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            {
                // WKWebView uses a nonpersistent datastore: no unmanaged WebKit
                // state outside the shared Library policy. History lives in KLP.
                let config = app
                    .config()
                    .app
                    .windows
                    .iter()
                    .find(|w| w.label == "main")
                    .unwrap();
                tauri::WebviewWindowBuilder::from_config(app, config)?
                    .incognito(true)
                    .build()?;
            }
            // Size first, while the window is still hidden.
            restore_window_size(app.handle());
            // Supervise the KLP connection for the app's lifetime:
            // connect, and reconnect whenever the agent's pipe drops
            // (the agent self-updates, so restarts are routine) — #468.
            tauri::async_runtime::spawn(supervise_connection(
                klp_slot.clone(),
                rejection.clone(),
                app.handle().clone(),
            ));
            // The window starts hidden (tauri.conf `visible: false`). On a
            // normal launch, reveal it immediately; on an emergency
            // launch, leave it hidden until the user clicks the toast
            // (`show_main_window`).
            if initially_visible {
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

#[cfg(test)]
mod desktop_tests {
    use super::*;

    #[test]
    fn native_notification_capability_stays_windows_only() {
        assert_eq!(
            automatic_notifications_available(),
            cfg!(target_os = "windows")
        );
    }
}
