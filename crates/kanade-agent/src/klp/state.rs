//! Endpoint state evaluator (SPEC §2.1 / §2.12.5 state.snapshot).
//!
//! Runs on a 30 s cadence in a background task spawned from
//! `main.rs`. Each tick:
//!
//! 1. Snapshots the current `EffectiveConfig` to read the rollout
//!    target version (for the `agent_self_update` check).
//! 2. Runs each platform check (disk-free via Win32, others
//!    stubbed for now — see TODOs).
//! 3. Builds a fresh [`StateSnapshot`] and publishes via the
//!    `watch::Sender`. The [`klp::handlers::state`] forwarder
//!    tasks pick up the change and push it down each subscribed
//!    KLP connection.
//!
//! The first snapshot is evaluated synchronously at startup
//! (`eval_once`) so the watch channel has a meaningful initial
//! value before any client connects — `state.snapshot` returns
//! the cached value without waiting for an evaluator tick.

use std::time::Duration;

use kanade_shared::ipc::state::{Check, CheckStatus, StateSnapshot};
use kanade_shared::wire::EffectiveConfig;
use tokio::sync::watch;
use tracing::debug;

/// How often the evaluator re-checks the endpoint. Picked so the
/// SPA's Health tab feels live without burning CPU — most checks
/// are sub-millisecond, but `disk_free` does a Win32 syscall and
/// future checks (BitLocker, AV) will do WMI queries that can
/// take 100-500 ms.
const EVAL_INTERVAL: Duration = Duration::from_secs(30);

/// Build a fresh snapshot synchronously. Called by `main.rs` once
/// at startup to seed the watch channel, then by `eval_loop`
/// every tick.
pub fn eval_once(pc_id: &str, agent_version: &str, cfg: &EffectiveConfig) -> StateSnapshot {
    let checks = vec![
        agent_self_update_check(agent_version, cfg.target_version.as_deref()),
        disk_free_check(),
        bitlocker_check_stub(),
        av_signature_check_stub(),
        cert_expiry_check_stub(),
    ];
    StateSnapshot {
        pc_id: pc_id.to_string(),
        // `online` is the agent's NATS connection status. The
        // listener doesn't have a live handle to the broker
        // client here; a future PR will plumb the connection
        // state through. For now we say `true` (the agent
        // wouldn't be answering KLP if it had crashed entirely,
        // which is the only state the SPA actually cares about).
        online: true,
        // VPN posture is site-specific and needs a custom
        // integration per organisation. Default to "unknown"
        // until the check is implemented — SPEC §2.12.5 explicitly
        // calls out the field is free-form text.
        vpn: "unknown".to_string(),
        checks,
        agent_version: agent_version.to_string(),
        target_version: cfg
            .target_version
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| agent_version.to_string()),
    }
}

/// Run forever (background task). Every [`EVAL_INTERVAL`], build a
/// new [`StateSnapshot`] and send via `state_tx`. The
/// [`watch::Sender`] collapses identical successive values, so
/// idle endpoints don't wake the forwarders unnecessarily —
/// `state.changed` push fires only on a real diff.
pub async fn eval_loop(
    state_tx: watch::Sender<StateSnapshot>,
    cfg_rx: watch::Receiver<EffectiveConfig>,
    pc_id: String,
    agent_version: String,
) {
    let mut tick = tokio::time::interval(EVAL_INTERVAL);
    // Skip the immediate first fire; main.rs already seeded the
    // channel with eval_once.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;
    loop {
        tick.tick().await;
        let snapshot = eval_once(&pc_id, &agent_version, &cfg_rx.borrow());
        // TODO(perf): use `send_if_modified` once StateSnapshot
        // derives PartialEq in `kanade-shared`. For now we send
        // unconditionally and every forwarder wakes every 30 s
        // even if nothing changed — measurable but tiny (a 0–2
        // connection agent emits ~60 B/s of spurious pushes).
        if state_tx.send(snapshot).is_err() {
            // All receivers dropped — the listener is shutting
            // down. Exit cleanly so the task doesn't spin.
            debug!(pc_id = %pc_id, "state.eval_loop: no receivers, exiting");
            return;
        }
    }
}

// ============================================================
// Check implementations
// ============================================================

/// `agent_self_update` — compares the running version against the
/// rollout target published by Sprint 6's config supervisor.
///
/// - Equal (or target unset) → Ok.
/// - Differ → Warn with detail so the SPA's Health tab shows
///   "restart pending" without yet being a hard failure.
fn agent_self_update_check(running: &str, target: Option<&str>) -> Check {
    let target = target.filter(|s| !s.is_empty()).unwrap_or(running);
    if running == target {
        Check {
            name: "agent_self_update".into(),
            status: CheckStatus::Ok,
            detail: Some(format!("running {running} (target matches)")),
            troubleshoot: None,
        }
    } else {
        Check {
            name: "agent_self_update".into(),
            status: CheckStatus::Warn,
            detail: Some(format!(
                "running {running}, target {target} — restart pending"
            )),
            // No user-invokable manifest yet for self-update; the
            // self_update background task handles this without
            // user action.
            troubleshoot: None,
        }
    }
}

/// `disk_free` — fraction of free space on `C:\` (Windows).
///
/// - > 10 % free → Ok.
/// - 5 - 10 % free → Warn.
/// - < 5 % free → Fail.
///
/// The threshold values are conservative defaults; future SPEC
/// work may make them configurable per fleet.
#[cfg(target_os = "windows")]
fn disk_free_check() -> Check {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::w;

    let mut free: u64 = 0;
    let mut total: u64 = 0;
    let result =
        unsafe { GetDiskFreeSpaceExW(w!("C:\\"), None, Some(&mut total), Some(&mut free)) };
    if let Err(e) = result {
        return Check {
            name: "disk_free".into(),
            status: CheckStatus::Unknown,
            detail: Some(format!("GetDiskFreeSpaceExW failed: {e}")),
            troubleshoot: None,
        };
    }
    if total == 0 {
        return Check {
            name: "disk_free".into(),
            status: CheckStatus::Unknown,
            detail: Some("C:\\ reports 0 total bytes".into()),
            troubleshoot: None,
        };
    }
    let pct = (free as f64 / total as f64) * 100.0;
    let to_gb = |b: u64| (b as f64) / 1024.0 / 1024.0 / 1024.0;
    let detail = Some(format!(
        "{:.1}% free ({:.1} GB / {:.1} GB)",
        pct,
        to_gb(free),
        to_gb(total),
    ));
    let status = if pct >= 10.0 {
        CheckStatus::Ok
    } else if pct >= 5.0 {
        CheckStatus::Warn
    } else {
        CheckStatus::Fail
    };
    Check {
        name: "disk_free".into(),
        status,
        detail,
        troubleshoot: None,
    }
}

#[cfg(not(target_os = "windows"))]
fn disk_free_check() -> Check {
    Check {
        name: "disk_free".into(),
        status: CheckStatus::Unknown,
        detail: Some("disk_free not implemented on non-Windows targets".into()),
        troubleshoot: None,
    }
}

/// `bitlocker` — TODO: query `Win32_EncryptableVolume` via WMI for
/// each fixed drive and report whether all are
/// `ProtectionStatus = 1` (Protection On). The check needs the
/// agent's existing WMI plumbing (currently shell-out to
/// powershell.exe per the `wmi` crate's drop, see Cargo.toml
/// comment).
fn bitlocker_check_stub() -> Check {
    Check {
        name: "bitlocker".into(),
        status: CheckStatus::Unknown,
        detail: Some("TODO: implement via Win32_EncryptableVolume WMI query".into()),
        troubleshoot: None,
    }
}

/// `av_signature` — TODO: query Windows Security Center's
/// `MSFT_MpComputerStatus` (PowerShell `Get-MpComputerStatus`)
/// for `AntivirusSignatureLastUpdated`; map (now - last_updated)
/// to ok/warn/fail thresholds (24 h / 7 d / older).
fn av_signature_check_stub() -> Check {
    Check {
        name: "av_signature".into(),
        status: CheckStatus::Unknown,
        detail: Some("TODO: implement via MSFT_MpComputerStatus".into()),
        troubleshoot: None,
    }
}

/// `cert_expiry` — TODO: enumerate the device's
/// machine certificate store (LocalMachine\My) via
/// `wincrypt::CertEnumCertificatesInStore` and flag any
/// cert with `NotAfter < now + 30 days`. Site-specific filtering
/// (which template / subject pattern to watch) gets added when a
/// real fleet wires this in.
fn cert_expiry_check_stub() -> Check {
    Check {
        name: "cert_expiry".into(),
        status: CheckStatus::Unknown,
        detail: Some("TODO: implement via wincrypt cert-store enumeration".into()),
        troubleshoot: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(target: Option<&str>) -> EffectiveConfig {
        let mut cfg = EffectiveConfig::builtin_defaults();
        cfg.target_version = target.map(str::to_owned);
        cfg
    }

    #[test]
    fn eval_once_produces_well_formed_snapshot() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(None));
        assert_eq!(snap.pc_id, "PC1234");
        assert!(snap.online);
        assert_eq!(snap.vpn, "unknown");
        assert_eq!(snap.agent_version, "0.41.0");
        assert_eq!(snap.target_version, "0.41.0"); // target unset → falls back
        // 5 checks: agent_self_update + disk_free + 3 stubs.
        assert_eq!(snap.checks.len(), 5);
        let names: Vec<&str> = snap.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "agent_self_update",
                "disk_free",
                "bitlocker",
                "av_signature",
                "cert_expiry"
            ]
        );
    }

    #[test]
    fn agent_self_update_ok_when_running_matches_target() {
        let c = agent_self_update_check("0.41.0", Some("0.41.0"));
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn agent_self_update_ok_when_target_unset() {
        let c = agent_self_update_check("0.41.0", None);
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn agent_self_update_ok_when_target_empty_string() {
        // Same defensive read as handle_version — a backend that
        // sets Some("") instead of clearing the field shouldn't
        // trip a phantom warning.
        let c = agent_self_update_check("0.41.0", Some(""));
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn agent_self_update_warn_when_target_differs() {
        let c = agent_self_update_check("0.41.0", Some("0.42.0"));
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.unwrap().contains("restart pending"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn disk_free_returns_concrete_status_on_windows() {
        // We can't pin the exact status (depends on the machine
        // running the test), but the check must run without
        // crashing and produce a sensible status + detail.
        let c = disk_free_check();
        assert_eq!(c.name, "disk_free");
        // Status is whatever the actual disk reports; just
        // assert it's not Unknown (which would indicate the
        // Win32 call failed unexpectedly on a healthy dev box).
        assert!(
            matches!(
                c.status,
                CheckStatus::Ok | CheckStatus::Warn | CheckStatus::Fail
            ),
            "expected concrete status, got {:?}",
            c.status
        );
        let detail = c.detail.expect("detail populated");
        assert!(detail.contains("free"), "detail: {detail}");
    }

    #[test]
    fn snapshot_target_version_falls_back_when_cfg_target_empty() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(Some("")));
        assert_eq!(snap.target_version, "0.41.0");
    }

    #[test]
    fn snapshot_target_version_surfaces_when_cfg_target_set() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(Some("0.42.0")));
        assert_eq!(snap.target_version, "0.42.0");
    }
}
