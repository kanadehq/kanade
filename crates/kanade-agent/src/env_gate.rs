//! #418 `constraints.require` — host-environment fire-time gate.
//!
//! The agent senses host state in-process (Windows: `GetSystemPowerStatus`
//! for AC, `WTSQuerySessionInformationW` for console idle,
//! `GetNetworkConnectivityHint` for internet connectivity, plus the
//! `host_perf` system CPU% sample) and feeds it to the pure decision fn
//! [`kanade_shared::manifest::require_met`]. Only the sensing is
//! platform-specific; the decision (and its tests) live in kanade-shared.
//! This is `runs_on: agent` only (validate rejects backend), evaluated as
//! a skip-this-tick gate in `local_scheduler::local_tick`.
//!
//! Not cfg-gated as a module: `local_tick` is cross-platform and calls
//! [`require_satisfied`] on every target; the Windows sensing is gated
//! internally and a non-Windows build returns "allow" (documented gap —
//! all kanade agents are Windows; decision K capability matrix).

use std::sync::atomic::{AtomicU32, Ordering};

use kanade_shared::manifest::Require;

/// Latest whole-machine CPU% (0–100), published by `host_perf_loop` each
/// tick. Stored as `f32` bits in an atomic; `f32::NAN` = "no sample yet"
/// (host_perf needs two samples before it reports CPU, and may be cold
/// on a freshly-started agent). Read by the `cpu_below` gate. Using the
/// continuously-sampled host_perf value is more accurate than a one-shot
/// read at gate time (sysinfo CPU% needs two samples to diff).
static LATEST_SYSTEM_CPU: AtomicU32 = AtomicU32::new(f32::NAN.to_bits());

/// Publish the latest system CPU% (called by `host_perf_loop`). `None`
/// (e.g. host_perf's first tick) records "unknown". Takes `f32` since
/// `sysinfo::global_cpu_usage()` is f32 — no precision is implied or lost.
pub fn set_system_cpu(pct: Option<f32>) {
    let bits = pct.unwrap_or(f32::NAN).to_bits();
    LATEST_SYSTEM_CPU.store(bits, Ordering::Relaxed);
}

/// The latest system CPU% (`None` = no sample yet → a `cpu_below`
/// requirement is treated as unmet, fail-closed).
// Only read in the Windows gate path; the non-Windows stub returns early.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn system_cpu() -> Option<f64> {
    let v = f32::from_bits(LATEST_SYSTEM_CPU.load(Ordering::Relaxed));
    if v.is_nan() { None } else { Some(v as f64) }
}

/// Fire-time env gate. An empty `require` short-circuits to `true` with
/// zero syscalls (the common case — most schedules have no require).
/// Windows: sense AC + idle + network, fold in the latest host CPU%,
/// apply `require_met`. Non-Windows: allow (sensing unsupported; no
/// non-Windows agents in the fleet).
pub fn require_satisfied(req: &Require) -> bool {
    if req.is_empty() {
        return true; // fast path — no Win32, no work
    }
    #[cfg(target_os = "windows")]
    {
        // Imported here (not at module scope) so non-Windows builds
        // don't flag them unused — the stub below never calls them.
        use kanade_shared::manifest::{EnvState, require_met};
        let (ac_online, idle, network_up) = sense_windows();
        require_met(
            req,
            &EnvState {
                ac_online,
                idle,
                cpu_pct: system_cpu(),
                network_up,
            },
        )
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

/// Sense `(ac_online, console_idle, network_up)` on Windows. `ac_online`
/// and `network_up` are fail-closed (`false`) when their status can't be
/// read — a restrictive gate must not fire when it can't confirm the
/// condition. `idle` is `None` when it can't be determined (so an idle
/// requirement is treated as unmet), EXCEPT a headless/disconnected
/// console (no interactive user) reports `Duration::MAX` — idle is then
/// trivially satisfied, since "don't run while the user is working" is
/// vacuously true with no one at the console.
#[cfg(target_os = "windows")]
fn sense_windows() -> (bool, Option<std::time::Duration>, bool) {
    use windows::Win32::NetworkManagement::IpHelper::GetNetworkConnectivityHint;
    use windows::Win32::Networking::WinSock::{
        NL_NETWORK_CONNECTIVITY_HINT, NetworkConnectivityLevelHintInternetAccess,
    };
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

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
    let idle = console_idle();

    // ---- network (internet connectivity) ----
    // SAFETY: `hint` is a valid, properly-aligned out-param the call only
    // writes into. On error / non-internet level we fail-closed (offline).
    // Note: unlike GetSystemPowerStatus, this API returns WIN32_ERROR
    // directly (not wrapped in windows::core::Result), so we match ret.0.
    let network_up = {
        let mut hint = NL_NETWORK_CONNECTIVITY_HINT::default();
        match unsafe { GetNetworkConnectivityHint(&mut hint) } {
            // 0 (NO_ERROR) on success. "Up" = full internet
            // (InternetAccess) ONLY. ConstrainedInternetAccess (captive
            // portal — traffic intercepted) is deliberately treated as
            // offline so a "don't run until online" download/phone-home
            // job isn't fired into a portal where it would just fail.
            // LocalAccess (LAN only), None, Unknown, Hidden → offline too.
            ret if ret.0 == 0 => {
                hint.ConnectivityLevel == NetworkConnectivityLevelHintInternetAccess
            }
            _ => false,
        }
    };

    (ac_online, idle, network_up)
}

/// Time since the last user input on the physical console.
/// `Some(Duration::MAX)` when there's no interactive console user (headless /
/// logged out); `Some(d)` for a real idle duration; `None` when it can't be
/// read. Factored out of [`sense_windows`] so the idle sampler (#841) can read
/// JUST idle each tick without the AC-power / network syscalls — and so the
/// fire-time gate keeps the exact same behaviour.
#[cfg(target_os = "windows")]
pub(crate) fn console_idle() -> Option<std::time::Duration> {
    use std::time::Duration;
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTSINFOW, WTSQuerySessionInformationW, WTSSessionInfo,
    };
    use windows::core::PWSTR;

    // SAFETY: no arguments; returns the physical console session id, or
    // 0xFFFFFFFF when no session is attached to the console.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == 0xFFFF_FFFF {
        // Headless / no console user → idle is vacuously satisfied.
        return Some(Duration::MAX);
    }
    let mut buf = PWSTR::null();
    let mut bytes: u32 = 0;
    // SAFETY: `WTSSessionInfo` returns a heap WTSINFOW into `buf` (size into
    // `bytes`). We read it through a `*const WTSINFOW` only after confirming
    // the byte count, then free it with `WTSFreeMemory`. `buf`/`bytes` are
    // valid out-params.
    unsafe {
        match WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTS_INFO_CLASS(WTSSessionInfo.0),
            &mut buf,
            &mut bytes,
        ) {
            Ok(()) if (bytes as usize) >= std::mem::size_of::<WTSINFOW>() && !buf.is_null() => {
                let info = &*(buf.0 as *const WTSINFOW);
                // FILETIME 100ns ticks; idle = now - last input. saturating_sub
                // so an anomalous (e.g. clock-skew) ordering can't
                // overflow-panic in debug builds.
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
            Err(_) => {
                // In practice `buf` is still the null we initialised it to, so
                // this is a no-op — but the docs don't guarantee a failed call
                // leaves `ppBuffer` untouched, so mirror the Ok-arm free for
                // symmetry rather than assume.
                if !buf.is_null() {
                    WTSFreeMemory(buf.0 as *mut core::ffi::c_void);
                }
                None
            }
        }
    }
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
            cpu_below: None,
            network: false,
        }));
        // Non-Windows: a non-empty require also returns true (allow-all
        // stub — the production fleet is all-Windows, decision K). Pins
        // that contract so a future refactor can't silently flip it.
        #[cfg(not(target_os = "windows"))]
        assert!(require_satisfied(&Require {
            ac_power: true,
            idle: Some("10m".into()),
            cpu_below: Some(20.0),
            network: true,
        }));
    }

    #[test]
    fn system_cpu_roundtrips_and_unknown_is_none() {
        // 42.5 is exactly representable in f32, so the f32-store → f64-read
        // round-trip is lossless here.
        set_system_cpu(Some(42.5));
        assert_eq!(system_cpu(), Some(42.5));
        set_system_cpu(None);
        assert!(system_cpu().is_none());
    }
}
