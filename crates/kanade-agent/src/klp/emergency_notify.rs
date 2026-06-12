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

use tracing::{debug, warn};

/// Path of the installed Client App under `%ProgramFiles%` — set by the
/// `install-kanade-client` job (`<ProgramFiles>\Kanade\kanade-client.exe`).
const CLIENT_EXE_REL: &str = r"Kanade\kanade-client.exe";

/// CLI flag the launched client reads to show the emergency as a toast
/// (hidden window) instead of its normal visible startup. Kept in sync
/// with the client's arg parser (`kanade-client`'s `app.rs`).
const SHOW_NOTIFICATION_ARG: &str = "--show-notification";

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

/// Surface an emergency notification by launching the Client App in the
/// user's session to show a toast for it. Fire-and-forget: spawns a
/// blocking thread for the WTS/CreateProcessAsUser dance and returns
/// immediately so the caller (the notify bus loop) is never blocked. A
/// no-op (logged) when the client isn't installed or the launch fails —
/// never panics, never propagates.
pub fn surface_emergency(notification_id: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Throttle: launching the heavy Tauri/WebView client is expensive, so
    // a burst of emergencies (or rapid re-sends) must not spawn it over
    // and over. At most one launch per cooldown window. (The bus calls
    // this serially, so a plain load/store is race-free here; the
    // client-side single-instance guard is tracked in #624.)
    const COOLDOWN_SECS: u64 = 10;
    static LAST_LAUNCH_SECS: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now.saturating_sub(LAST_LAUNCH_SECS.load(Ordering::Relaxed)) < COOLDOWN_SECS {
        debug!("emergency fallback: client launch throttled (cooldown active)");
        return;
    }
    LAST_LAUNCH_SECS.store(now, Ordering::Relaxed);

    let id = notification_id.to_owned();
    // Off the async bus task entirely (the Win32 spawn is sync).
    tokio::task::spawn_blocking(move || {
        let Some(exe) = client_exe_path() else {
            debug!("emergency fallback: Client App not installed; nothing to launch");
            return;
        };
        match crate::process_as_user::launch_detached_in_user_session(
            &exe,
            &[SHOW_NOTIFICATION_ARG, &id],
        ) {
            Ok(()) => debug!(
                notification_id = %id,
                "emergency fallback: launched Client App to toast the emergency",
            ),
            Err(e) => warn!(
                error = %e,
                notification_id = %id,
                "emergency fallback: failed to launch Client App",
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `surface_emergency` must be panic-safe and non-blocking no matter
    /// the environment. CI runners have no installed client / console
    /// session, so it no-ops on the spawned blocking thread — the call
    /// still returns immediately and never propagates. (The real launch
    /// can't be asserted without an installed client + logged-in
    /// session; this guards the call path.)
    #[tokio::test]
    async fn surface_emergency_is_noop_safe_without_client() {
        surface_emergency("notif-9f3a");
        // Let the spawned blocking task get scheduled; it must not panic.
        tokio::task::yield_now().await;
    }
}
