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

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

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

/// #855: latest console idle reported by the in-session `session-agent`
/// (`--session-agent` child), which reads `GetLastInputInfo` *inside* the
/// user session. The SYSTEM agent can't read truthful per-session idle
/// (WTS `LastInputTime` is stale for an active console session, and
/// `GetLastInputInfo` is session-affine), so it consumes this cache instead.
/// `(set_at, idle)`; `console_idle()` returns it while fresh, else MAX.
static LATEST_CONSOLE_IDLE: Mutex<Option<(Instant, Duration)>> = Mutex::new(None);

/// How often the session-agent samples idle in-session (also the child's
/// stdout cadence). Shared so the freshness window below stays in step.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const SESSION_IDLE_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// A cached idle older than this is stale (session-agent down / no console
/// user) → `console_idle()` falls back to `Duration::MAX` (= idle, fail-safe).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const SESSION_IDLE_STALE_AFTER: Duration = Duration::from_secs(35); // ~3.5× sample

/// Publish the latest in-session idle, called by the session supervisor as it
/// reads the session-agent's stdout. `None` clears it (no session / agent
/// down) so `console_idle()` falls back to MAX (treat as idle).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn set_console_idle(idle: Option<Duration>) {
    let mut g = LATEST_CONSOLE_IDLE.lock().unwrap_or_else(|e| e.into_inner());
    *g = idle.map(|d| (Instant::now(), d));
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

/// Time since the last user input on the active console session.
///
/// #855: reads the cache fed by the in-session `session-agent` (which calls
/// `GetLastInputInfo` *inside* the user session — the only truthful source).
/// It does NOT read WTS `LastInputTime` anymore: that value is stale for an
/// active console session (a working user reads as days-idle), which froze the
/// #841 active lane and broke the #418 idle gate.
///
/// `Some(d)` while the cache is fresh; otherwise `Some(Duration::MAX)` —
/// stale (session-agent down), no console user, or not yet sampled all map to
/// "idle". This keeps both consumers (the idle sampler and the require gate)
/// on the idle side (fail-safe) and matches the prior headless contract
/// ("don't run while the user is working" is vacuously true with no one there).
/// Never returns `None`: a `None` would split semantics between the gate
/// (fail-closed) and `idle_sampler::step` (holds state), so MAX is used for
/// every unknown.
#[cfg(target_os = "windows")]
pub(crate) fn console_idle() -> Option<std::time::Duration> {
    let g = LATEST_CONSOLE_IDLE.lock().unwrap_or_else(|e| e.into_inner());
    match *g {
        Some((set_at, idle)) if set_at.elapsed() <= SESSION_IDLE_STALE_AFTER => Some(idle),
        _ => Some(Duration::MAX),
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

    // #855: console_idle reads the session-agent cache, and an absent / cleared
    // value falls back to MAX (idle) rather than None. (The stale-by-age branch
    // is time-based and not exercised here.)
    #[cfg(target_os = "windows")]
    #[test]
    fn console_idle_returns_fresh_cache_then_max_when_cleared() {
        use std::time::Duration;
        set_console_idle(Some(Duration::from_secs(3)));
        assert_eq!(console_idle(), Some(Duration::from_secs(3)));
        set_console_idle(None);
        assert_eq!(console_idle(), Some(Duration::MAX));
    }
}
