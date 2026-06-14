//! Native emergency surfacing — the fallback for when an `emergency`
//! notification arrives but no Client App is subscribed to receive the
//! live push (KLP Phase E, #102).
//!
//! The notification bus is at-most-once (SPEC §2.12.7): a notification
//! published while no Client App is connected is normally dropped and
//! recovered later via `notifications.list`. That's fine for info/warn,
//! but an **emergency** whose whole point is to grab attention *now*
//! (SPEC §2.12.8) must not wait for the user to happen to open the app —
//! especially since the Client App does not autostart on logon, so
//! "no client connected" is a common state.
//!
//! So when [`crate::klp::notify_bus`] sees an `emergency` with zero
//! broadcast receivers, it calls [`surface_emergency`], which **launches
//! the Client App** in the active console session (via
//! [`crate::process_as_user::launch_detached_in_user_session`], the same
//! WTS token path the agent already uses for `run_as: user`), passing the
//! notification id. The launched client starts **hidden** (no window) and
//! shows only a native **toast** for the emergency — so it never
//! "bursts" over whatever the user is doing (e.g. a meeting). Clicking
//! the toast is what brings the window forward, focused on the
//! notification panel.
//!
//! We deliberately do NOT pop a `WTSSendMessageW` message box here: a
//! blocking system dialog is exactly the screen-takeover the toast design
//! avoids.
//!
//! Best-effort: the launch is fire-and-forget on a blocking thread so the
//! bus loop never stalls; a missing client install or a spawn failure is
//! a logged no-op, never propagated.

#![cfg(target_os = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use kanade_shared::manifest::OnTrigger;
use tracing::{debug, info, warn};

/// Path of the installed Client App under `%ProgramFiles%` — set by the
/// `install-kanade-client` job (`<ProgramFiles>\Kanade\kanade-client.exe`).
const CLIENT_EXE_REL: &str = r"Kanade\kanade-client.exe";

/// CLI flag the launched client reads to show the emergency as a toast
/// (hidden window) instead of its normal visible startup. Kept in sync
/// with the client's arg parser (`kanade-client`'s `app.rs`).
const SHOW_NOTIFICATION_ARG: &str = "--show-notification";

/// CLI flag telling the client to **re-toast every still-unread, unexpired
/// emergency** (hidden window), bypassing its in-app duplicate-suppression —
/// used to re-pop emergencies the user couldn't see when they arrived (sent
/// while signed out, or delivered to the Action Center while the screen was
/// locked). Whether the client is launched fresh or this is forwarded to an
/// already-running instance, it re-pops. Kept in sync with `app.rs`.
const RESURFACE_ARG: &str = "--resurface";

/// Resolve the installed Client App path, or `None` when it isn't
/// installed (so the fallback is a clean no-op rather than spawning a
/// missing exe).
fn client_exe_path() -> Option<PathBuf> {
    // Prefer ProgramW6432 (always the 64-bit Program Files, even from a
    // 32-bit process) and fall back to ProgramFiles, so a 32-bit agent
    // build still finds the 64-bit-installed client.
    let program_files =
        std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles"))?;
    let path = PathBuf::from(program_files).join(CLIENT_EXE_REL);
    path.exists().then_some(path)
}

/// Launch throttle window: launching the heavy Tauri/WebView client is
/// expensive, so a burst of emergencies (or a logon racing a fallback) must
/// not spawn it over and over. At most one launch per window.
const COOLDOWN_SECS: u64 = 10;
static LAST_LAUNCH_SECS: AtomicU64 = AtomicU64::new(0);

/// Set when an emergency was processed while the user was **not present** —
/// signed out (no console session) or signed in but **locked** — so the toast
/// either couldn't be shown or went silently to the Action Center. Re-popped
/// the next time the user becomes present (logon or unlock; see
/// [`on_session_event`]). In-memory only: an agent restart loses it, but the
/// emergency is still in the 90-day NOTIFICATIONS stream and recovers when the
/// user next opens the Client App by hand (#647).
static PENDING_RESURFACE: AtomicBool = AtomicBool::new(false);

/// Whether the console session is currently **locked**. Tracked from the SCM
/// `SessionLock` / `SessionUnlock` events ([`on_session_event`]). Defaults to
/// unlocked: if the agent (re)starts while locked we won't know until the next
/// event, an accepted edge.
static LOCKED: AtomicBool = AtomicBool::new(false);

/// True when an interactive user is attached to the physical console.
/// `WTSGetActiveConsoleSessionId` returns `0xFFFFFFFF` when none is (no
/// signed-in user) — in which case there's nobody to toast at.
fn has_active_console_session() -> bool {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    // SAFETY: no arguments; the call only reads OS state.
    unsafe { WTSGetActiveConsoleSessionId() != 0xFFFF_FFFF }
}

/// True when a user can actually *see* a toast right now: signed in AND not
/// locked. When false, an emergency must be re-popped on the next presence.
fn is_present() -> bool {
    has_active_console_session() && !LOCKED.load(Ordering::Relaxed)
}

/// Launch the installed Client App in the active user session, fire-and-forget
/// on a **detached OS thread** — so it works whether the caller is on the
/// tokio runtime (the notify bus) or the SCM control thread
/// ([`on_session_event`], which runs outside any runtime). `args` pass through
/// to the client; throttled to one launch per [`COOLDOWN_SECS`]. A missing
/// install or spawn failure is a logged no-op, never panics, never propagates.
fn launch_client(args: Vec<String>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now.saturating_sub(LAST_LAUNCH_SECS.load(Ordering::Relaxed)) < COOLDOWN_SECS {
        debug!("emergency fallback: client launch throttled (cooldown active)");
        return;
    }
    LAST_LAUNCH_SECS.store(now, Ordering::Relaxed);

    std::thread::spawn(move || {
        let Some(exe) = client_exe_path() else {
            debug!("emergency fallback: Client App not installed; nothing to launch");
            return;
        };
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match crate::process_as_user::launch_detached_in_user_session(&exe, &arg_refs) {
            Ok(()) => debug!(?args, "emergency fallback: launched Client App"),
            Err(e) => warn!(error = %e, ?args, "emergency fallback: failed to launch Client App"),
        }
    });
}

/// Surface an emergency notification by launching the Client App in the user's
/// session to toast it (the **no-subscribed-client fallback**). Fire-and-forget;
/// never panics, never propagates.
///
/// Presence-aware (#647):
/// - **Signed out** (no console session) → nobody to toast at, so flag for
///   re-pop on the next logon instead of launching into the void.
/// - **Locked** → still launch (the toast lands in the Action Center as a
///   backstop) *and* flag for re-pop on unlock, since a toast that arrives
///   while locked is never actively shown.
/// - **Present** → launch and the toast shows immediately.
pub fn surface_emergency(notification_id: &str) {
    if !has_active_console_session() {
        PENDING_RESURFACE.store(true, Ordering::Relaxed);
        info!(
            notification_id,
            "emergency fallback: no signed-in user — deferring surface to next logon",
        );
        return;
    }
    if LOCKED.load(Ordering::Relaxed) {
        PENDING_RESURFACE.store(true, Ordering::Relaxed);
        info!(
            notification_id,
            "emergency fallback: screen locked — toasting to Action Center, will re-pop on unlock",
        );
    }
    launch_client(vec![
        SHOW_NOTIFICATION_ARG.to_string(),
        notification_id.to_owned(),
    ]);
}

/// Note that an emergency was **live-pushed to an already-connected client**
/// (so [`surface_emergency`]'s fallback didn't run). If the user isn't present
/// — i.e. locked — the client's toast went silently to the Action Center, so
/// flag it for re-pop on the next presence. Called from the notify bus for
/// every emergency that reaches a subscriber.
pub fn note_emergency_live_pushed() {
    if !is_present() {
        PENDING_RESURFACE.store(true, Ordering::Relaxed);
    }
}

/// React to an OS session event (#647). On **logon** or **unlock** — i.e. the
/// user becoming present — if an emergency was deferred while they were away,
/// launch the client with `--resurface` so it re-pops every still-unread,
/// unexpired emergency (`info`/`warn` stay passive by design). Also tracks the
/// lock state. A no-op when nothing was deferred.
pub fn on_session_event(trigger: OnTrigger) {
    match trigger {
        OnTrigger::Lock => {
            LOCKED.store(true, Ordering::Relaxed);
        }
        OnTrigger::Unlock => {
            LOCKED.store(false, Ordering::Relaxed);
            resurface_if_pending("unlock");
        }
        OnTrigger::Logon => {
            // A fresh logon is necessarily unlocked. Clear any stale `LOCKED`
            // (e.g. a `Lock` seen with no matching `Unlock` before this logon)
            // so `is_present()` doesn't wrongly defer every later live-pushed
            // emergency.
            LOCKED.store(false, Ordering::Relaxed);
            resurface_if_pending("logon");
        }
        _ => {}
    }
}

/// If an emergency was deferred while the user was away, re-pop it now that
/// they're present.
fn resurface_if_pending(event: &str) {
    if PENDING_RESURFACE.swap(false, Ordering::Relaxed) {
        info!(
            event,
            "emergency fallback: user present again — re-surfacing deferred emergency"
        );
        // A locked-screen emergency calls `launch_client` (toasting to the
        // Action Center) and so DID set the cooldown; if the user unlocks
        // within COOLDOWN_SECS this presence-driven re-pop would be throttled.
        // (The no-session path returns before `launch_client`, so it doesn't
        // set the cooldown — but resetting here is harmless and covers the
        // locked case.) Reset so the re-pop always gets through.
        LAST_LAUNCH_SECS.store(0, Ordering::Relaxed);
        launch_client(vec![RESURFACE_ARG.to_string()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `surface_emergency` must be panic-safe and non-blocking no matter the
    /// environment. CI runners have no console session, so it takes the
    /// "defer to logon" branch (sets the flag, no launch); a dev box with a
    /// session but no installed client no-ops the launch on a detached
    /// thread. Either way the call returns immediately and never propagates.
    /// (The real launch can't be asserted without an installed client + a
    /// logged-in session; this guards the call path.)
    #[test]
    fn surface_emergency_is_noop_safe() {
        surface_emergency("notif-9f3a");
    }

    /// `on_session_event` must be panic-safe for every trigger, whether or
    /// not an emergency was deferred. (The flag swap + launch are exercised
    /// here; the launch is a no-op without an installed client. We avoid
    /// asserting on the process-global `PENDING_ON_LOGON` because cargo runs
    /// these tests in parallel and `surface_emergency` above can flip it.)
    #[test]
    fn on_session_event_is_noop_safe_for_all_triggers() {
        note_emergency_live_pushed();
        on_session_event(OnTrigger::Logon);
        on_session_event(OnTrigger::Lock);
        on_session_event(OnTrigger::Unlock);
    }
}
