//! #418 `constraints.require` — host-environment fire-time gate.
//!
//! The agent senses host state in-process (Windows: `GetSystemPowerStatus`
//! for AC, `WTSQuerySessionInformationW` for console idle) and feeds it to
//! the pure decision fn [`kanade_shared::manifest::require_met`]. Only the
//! sensing is platform-specific; the decision (and its tests) live in
//! kanade-shared. This is `runs_on: agent` only (validate rejects backend),
//! evaluated as a skip-this-tick gate in `local_scheduler::local_tick`.
//!
//! Not cfg-gated as a module: `local_tick` is cross-platform and calls
//! [`require_satisfied`] on every target; the Windows sensing is gated
//! internally and a non-Windows build returns "allow" (documented gap —
//! all kanade agents are Windows; decision K capability matrix).

use kanade_shared::manifest::Require;

/// Fire-time env gate. An empty `require` short-circuits to `true` with
/// zero syscalls (the common case — most schedules have no require).
/// Windows: sense AC + idle and apply `require_met`. Non-Windows: allow
/// (sensing unsupported; no non-Windows agents in the fleet).
pub fn require_satisfied(req: &Require) -> bool {
    if req.is_empty() {
        return true; // fast path — no Win32, no work
    }
    #[cfg(target_os = "windows")]
    {
        // Imported here (not at module scope) so non-Windows builds
        // don't flag it unused — the stub below never calls it.
        use kanade_shared::manifest::require_met;
        let (ac_online, idle) = sense_windows();
        require_met(req, ac_online, idle)
    }
    #[cfg(not(target_os = "windows"))]
    {
        // No host sensing off Windows. Allow rather than fail-closed so a
        // non-Windows build (CI, dev) doesn't permanently starve every
        // require-gated schedule; the production fleet is all-Windows.
        let _ = req;
        true
    }
}

/// Sense `(ac_online, console_idle)` on Windows. `ac_online` is
/// fail-closed (`false`) when the power status can't be read — a
/// restrictive gate must not fire when it can't confirm the condition.
/// `idle` is `None` when it can't be determined (so an idle requirement
/// is treated as unmet), EXCEPT a headless/disconnected console (no
/// interactive user) reports `Duration::MAX` — idle is then trivially
/// satisfied, since "don't run while the user is working" is vacuously
/// true with no one at the console.
#[cfg(target_os = "windows")]
fn sense_windows() -> (bool, Option<std::time::Duration>) {
    use std::time::Duration;
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTSINFOW, WTSQuerySessionInformationW, WTSSessionInfo,
    };
    use windows::core::PWSTR;

    // ---- AC power ----
    // SAFETY: `st` is a valid, properly-aligned SYSTEM_POWER_STATUS; the
    // call only writes into it. On error we fail-closed (not on AC).
    let ac_online = {
        let mut st = SYSTEM_POWER_STATUS::default();
        match unsafe { GetSystemPowerStatus(&mut st) } {
            // 1 = online (AC); 0 = offline (battery); 255 = unknown.
            Ok(()) => st.ACLineStatus == 1,
            Err(_) => false,
        }
    };

    // ---- console idle ----
    let idle = {
        // SAFETY: no arguments; returns the physical console session id,
        // or 0xFFFFFFFF when no session is attached to the console.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == 0xFFFF_FFFF {
            // Headless / no console user → idle is vacuously satisfied.
            Some(Duration::MAX)
        } else {
            let mut buf = PWSTR::null();
            let mut bytes: u32 = 0;
            // SAFETY: `WTSSessionInfo` returns a heap WTSINFOW into `buf`
            // (size into `bytes`). We read it through a `*const WTSINFOW`
            // only after confirming the byte count, then free it with
            // `WTSFreeMemory`. `buf`/`bytes` are valid out-params.
            unsafe {
                match WTSQuerySessionInformationW(
                    Some(WTS_CURRENT_SERVER_HANDLE),
                    session,
                    WTS_INFO_CLASS(WTSSessionInfo.0),
                    &mut buf,
                    &mut bytes,
                ) {
                    Ok(())
                        if (bytes as usize) >= std::mem::size_of::<WTSINFOW>()
                            && !buf.is_null() =>
                    {
                        let info = &*(buf.0 as *const WTSINFOW);
                        // FILETIME 100ns ticks; idle = now - last input.
                        // saturating_sub so an anomalous (e.g. clock-skew)
                        // ordering can't overflow-panic in debug builds.
                        let delta_100ns = info.CurrentTime.saturating_sub(info.LastInputTime);
                        let d = if delta_100ns > 0 {
                            Duration::from_nanos((delta_100ns as u64).saturating_mul(100))
                        } else {
                            Duration::ZERO
                        };
                        WTSFreeMemory(buf.0 as *mut core::ffi::c_void);
                        Some(d)
                    }
                    Ok(()) => {
                        // Short/empty buffer — free it if any, report unknown.
                        if !buf.is_null() {
                            WTSFreeMemory(buf.0 as *mut core::ffi::c_void);
                        }
                        None
                    }
                    Err(_) => None,
                }
            }
        }
    };

    (ac_online, idle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_require_is_satisfied_without_syscalls() {
        // Exercises the no-Win32 fast path on every platform.
        assert!(require_satisfied(&Require::default()));
        assert!(require_satisfied(&Require {
            ac_power: false,
            idle: None,
        }));
        // Non-Windows: a non-empty require also returns true (allow-all
        // stub — the production fleet is all-Windows, decision K). Pins
        // that contract so a future refactor can't silently flip it.
        #[cfg(not(target_os = "windows"))]
        assert!(require_satisfied(&Require {
            ac_power: true,
            idle: Some("10m".into()),
        }));
    }
}
