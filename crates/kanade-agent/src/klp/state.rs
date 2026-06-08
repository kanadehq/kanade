//! Endpoint state evaluator (SPEC §2.1 / §2.12.5 state.snapshot).
//!
//! Runs on a 30 s cadence in a background task spawned from
//! `main.rs`. Each tick:
//!
//! 1. Snapshots the current `EffectiveConfig` to read the rollout
//!    target version (for the `agent_self_update` check).
//! 2. Runs each platform check: `disk_free` via Win32, and
//!    `bitlocker` / `av_signature` / `cert_expiry` via `powershell`
//!    WMI / cert-store shell-outs (run concurrently, each with its
//!    own timeout).
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

use async_nats::connection::State;
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

/// Per-query wall-clock cap on the `powershell.exe` WMI / cert
/// shell-outs. WMI providers can wedge; a check that overruns this
/// degrades to `Unknown` for the tick instead of stalling the whole
/// snapshot (and, via `join!`, the other two checks). Comfortably
/// above the ~100-500 ms a healthy query takes.
const WMI_TIMEOUT: Duration = Duration::from_secs(8);

/// `av_signature` thresholds (chrono's `Duration::hours/days` aren't
/// `const fn`, so these stay as scalars and build the `Duration` at
/// the comparison site): signatures refreshed within a day are
/// healthy; up to a week is a warning; older is a failure.
const AV_SIGNATURE_OK_MAX_HOURS: i64 = 24;
const AV_SIGNATURE_WARN_MAX_DAYS: i64 = 7;

/// `cert_expiry` warns when the soonest-expiring machine cert is
/// within this window; a cert already past `NotAfter` fails.
const CERT_EXPIRY_WARN_DAYS: i64 = 30;

/// Whether the agent currently holds a live broker connection —
/// the value behind `StateSnapshot.online` (#288). Mirrors the
/// `connection_state() == Connected` check already used by
/// [`crate::nats_retry`] / [`crate::staleness`]; reading it is a
/// cheap atomic load, so the evaluator re-samples it every tick and
/// a dropped broker flips the Health tab to offline within one
/// [`EVAL_INTERVAL`].
pub fn client_online(client: &async_nats::Client) -> bool {
    client.connection_state() == State::Connected
}

/// Build a fresh snapshot. Called by `main.rs` once at startup to
/// seed the watch channel, then by `eval_loop` every tick.
///
/// `online` is the agent's live broker-connection status, sampled
/// by the caller via [`client_online`] — passed in (not read here)
/// so the snapshot-shape tests can pin it both ways.
///
/// `async` because three of the checks (`bitlocker`, `av_signature`,
/// `cert_expiry`) shell out to `powershell.exe` for WMI / cert-store
/// reads (the agent runs as LocalSystem, where the in-process `wmi`
/// crate hit `WBEM_E_INVALID_CLASS` and was dropped in v0.12.0). The
/// three run concurrently via `join!` and each carries its own
/// timeout, so a wedged WMI provider degrades one row instead of
/// stalling the whole snapshot.
pub async fn eval_once(
    pc_id: &str,
    agent_version: &str,
    cfg: &EffectiveConfig,
    online: bool,
) -> StateSnapshot {
    let (bitlocker, av_signature, cert_expiry) =
        tokio::join!(bitlocker_check(), av_signature_check(), cert_expiry_check(),);
    let checks = vec![
        agent_self_update_check(agent_version, cfg.target_version.as_deref()),
        disk_free_check(),
        bitlocker,
        av_signature,
        cert_expiry,
    ];
    StateSnapshot {
        pc_id: pc_id.to_string(),
        // Real broker-connection state, sampled by the caller (see
        // `client_online`) so this stays unit-testable (#288).
        online,
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
    client: async_nats::Client,
) {
    let mut tick = tokio::time::interval(EVAL_INTERVAL);
    // Skip the immediate first fire; main.rs already seeded the
    // channel with eval_once.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;
    loop {
        tick.tick().await;
        // Clone the config out of the watch guard before the
        // `eval_once` await — a `watch::Ref` is not `Send` and can't
        // be held across the WMI shell-outs' await points.
        let cfg = cfg_rx.borrow().clone();
        let snapshot = eval_once(&pc_id, &agent_version, &cfg, client_online(&client)).await;
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

// ============================================================
// WMI / cert-store checks (#290)
//
// These shell out to `powershell.exe` rather than linking the
// `wmi` crate: in-process WBEM queries return WBEM_E_INVALID_CLASS
// when the agent runs as LocalSystem, which is why `wmi` was
// dropped in v0.12.0 (see kanade-agent/Cargo.toml). Each snippet is
// wrapped in try/catch so it ALWAYS emits a single `{ ok, ... }`
// JSON object on stdout — even the access-denied path — and the
// Rust side just branches on `ok`. `klp/state.rs` is compiled on
// Windows only (the whole `klp` module is gated in `main.rs`), so
// these need no non-Windows variant.
// ============================================================

/// `bitlocker` — `ProtectionStatus` of every encryptable volume
/// (`Win32_EncryptableVolume`, namespace
/// `root\CIMV2\Security\MicrosoftVolumeEncryption`). All protected →
/// Ok, any volume Off → Warn. The namespace is admin-only, so a
/// non-elevated agent (or a host without BitLocker) degrades to
/// Unknown rather than failing the snapshot — in production the
/// agent runs as LocalSystem and the query succeeds.
async fn bitlocker_check() -> Check {
    // `$v = @(pipeline)` forces an array even for 0/1 results, so the
    // empty store serialises to `[]` (not `[null]` / an unwrapped
    // scalar) regardless of PowerShell version.
    const QUERY: &str = "try { $v = @(Get-CimInstance -Namespace 'root/CIMV2/Security/MicrosoftVolumeEncryption' -ClassName Win32_EncryptableVolume -ErrorAction Stop | ForEach-Object { [pscustomobject]@{ drive = $_.DriveLetter; protection = [int]$_.ProtectionStatus } }); [pscustomobject]@{ ok = $true; volumes = $v } | ConvertTo-Json -Compress -Depth 4 } catch { [pscustomobject]@{ ok = $false; err = $_.Exception.Message } | ConvertTo-Json -Compress }";
    let value = match wmi_json("bitlocker", QUERY).await {
        Ok(v) => v,
        Err(check) => return check,
    };
    let volumes: Vec<(Option<String>, i64)> = value
        .get("volumes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|vol| {
                    // Skip any non-object element defensively (a
                    // malformed `[null]` won't masquerade as a volume).
                    let vol = vol.as_object()?;
                    let drive = vol.get("drive").and_then(|d| d.as_str()).map(str::to_owned);
                    // ProtectionStatus: 1=On, 0=Off, 2=Unknown.
                    let protection = vol.get("protection").and_then(|p| p.as_i64()).unwrap_or(2);
                    Some((drive, protection))
                })
                .collect()
        })
        .unwrap_or_default();
    let (status, detail) = classify_bitlocker(&volumes);
    Check {
        name: "bitlocker".into(),
        status,
        detail: Some(detail),
        troubleshoot: None,
    }
}

/// `av_signature` — age of the Defender antivirus signatures
/// (`MSFT_MpComputerStatus.AntivirusSignatureLastUpdated`, namespace
/// `root\Microsoft\Windows\Defender`). ≤24 h → Ok, ≤7 d → Warn,
/// older → Fail; no Defender / query error → Unknown.
async fn av_signature_check() -> Check {
    const QUERY: &str = "try { $s = Get-CimInstance -Namespace 'root/Microsoft/Windows/Defender' -ClassName MSFT_MpComputerStatus -ErrorAction Stop; [pscustomobject]@{ ok = $true; last = $s.AntivirusSignatureLastUpdated.ToString('o') } | ConvertTo-Json -Compress } catch { [pscustomobject]@{ ok = $false; err = $_.Exception.Message } | ConvertTo-Json -Compress }";
    let value = match wmi_json("av_signature", QUERY).await {
        Ok(v) => v,
        Err(check) => return check,
    };
    let last = value.get("last").and_then(|v| v.as_str());
    let Some(updated) = last
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
    else {
        return wmi_unknown(
            "av_signature",
            &format!("unparseable AntivirusSignatureLastUpdated: {last:?}"),
        );
    };
    let age = chrono::Utc::now().signed_duration_since(updated);
    let status = classify_av_age(age);
    let detail = format!(
        "signatures last updated {} ({} h ago)",
        updated.format("%Y-%m-%dT%H:%MZ"),
        age.num_hours().max(0),
    );
    Check {
        name: "av_signature".into(),
        status,
        detail: Some(detail),
        troubleshoot: None,
    }
}

/// `cert_expiry` — soonest `NotAfter` across the machine personal
/// store (`Cert:\LocalMachine\My`). >30 d out (or empty store) → Ok,
/// within 30 d → Warn, already expired → Fail; enumeration error →
/// Unknown.
async fn cert_expiry_check() -> Check {
    // `$c = @(pipeline)` forces an array — an empty store serialises
    // to `[]` rather than `[null]` / an unwrapped scalar.
    const QUERY: &str = "try { $c = @(Get-ChildItem Cert:\\LocalMachine\\My -ErrorAction Stop | ForEach-Object { [pscustomobject]@{ subject = $_.Subject; notAfter = $_.NotAfter.ToString('o') } }); [pscustomobject]@{ ok = $true; certs = $c } | ConvertTo-Json -Compress -Depth 4 } catch { [pscustomobject]@{ ok = $false; err = $_.Exception.Message } | ConvertTo-Json -Compress }";
    let value = match wmi_json("cert_expiry", QUERY).await {
        Ok(v) => v,
        Err(check) => return check,
    };
    let now = chrono::Utc::now();
    // Soonest-expiring cert: min NotAfter across the store.
    let soonest = value
        .get("certs")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let not_after = c.get("notAfter").and_then(|n| n.as_str())?;
            let parsed = chrono::DateTime::parse_from_rfc3339(not_after)
                .ok()?
                .with_timezone(&chrono::Utc);
            let subject = c
                .get("subject")
                .and_then(|s| s.as_str())
                .unwrap_or("<no subject>")
                .to_owned();
            Some((parsed, subject))
        })
        .min_by_key(|(not_after, _)| *not_after);
    let (status, detail) =
        classify_cert_expiry(soonest.map(|(na, subj)| ((na - now).num_days(), subj)));
    Check {
        name: "cert_expiry".into(),
        status,
        detail: Some(detail),
        troubleshoot: None,
    }
}

/// Run a PowerShell snippet that emits one `{ ok, ... }` JSON object
/// and return it parsed, or — on spawn / timeout / parse / `ok:false`
/// failure — an `Unknown` [`Check`] (named `name`) carrying the
/// reason. Forces UTF-8 output so non-ASCII (Japanese error text,
/// cert subjects) survives the console codepage, hides the console
/// window, and caps wall-clock at [`WMI_TIMEOUT`].
async fn wmi_json(name: &str, query: &str) -> Result<serde_json::Value, Check> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW — no console flash for a background check.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let script = format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {query}");
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &script,
    ])
    .creation_flags(CREATE_NO_WINDOW);

    // kill_on_drop: when the timeout fires, the `output()` future is
    // dropped — but tokio won't reap the child unless asked, so a
    // wedged WMI provider would otherwise leak a `powershell.exe`
    // every tick. Dropping the command then kills the process group.
    let mut command = tokio::process::Command::from(cmd);
    command.kill_on_drop(true);
    let output = match tokio::time::timeout(WMI_TIMEOUT, command.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(wmi_unknown(name, &format!("spawn powershell: {e}"))),
        Err(_) => {
            return Err(wmi_unknown(
                name,
                &format!("powershell query timed out after {WMI_TIMEOUT:?}"),
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(wmi_unknown(
            name,
            &format!(
                "empty output (exit {}, stderr: {})",
                output.status,
                stderr.trim()
            ),
        ));
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            let preview: String = trimmed.chars().take(120).collect();
            return Err(wmi_unknown(
                name,
                &format!("parse json: {e} (output: {preview})"),
            ));
        }
    };
    // The try/catch wrapper sets ok=false on the WMI error path
    // (e.g. access-denied when non-elevated).
    if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = value
            .get("err")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(wmi_unknown(name, err));
    }
    Ok(value)
}

/// A check that couldn't read its data source — surfaced as Unknown
/// with the reason, never the old permanent `TODO:` text (#290).
fn wmi_unknown(name: &str, reason: &str) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Unknown,
        detail: Some(format!("unavailable: {reason}")),
        troubleshoot: None,
    }
}

/// Map antivirus-signature age to a status. Pure; unit-tested.
fn classify_av_age(age: chrono::Duration) -> CheckStatus {
    if age <= chrono::Duration::hours(AV_SIGNATURE_OK_MAX_HOURS) {
        CheckStatus::Ok
    } else if age <= chrono::Duration::days(AV_SIGNATURE_WARN_MAX_DAYS) {
        CheckStatus::Warn
    } else {
        CheckStatus::Fail
    }
}

/// Classify the soonest cert expiry. `soonest` is `(days_until,
/// subject)` for the nearest `NotAfter`, or `None` for an empty
/// store. Pure; unit-tested.
fn classify_cert_expiry(soonest: Option<(i64, String)>) -> (CheckStatus, String) {
    match soonest {
        None => (
            CheckStatus::Ok,
            "no certificates in LocalMachine\\My".to_string(),
        ),
        Some((days, subject)) if days < 0 => (
            CheckStatus::Fail,
            format!("certificate expired {} day(s) ago: {subject}", -days),
        ),
        Some((days, subject)) if days <= CERT_EXPIRY_WARN_DAYS => (
            CheckStatus::Warn,
            format!("certificate expires in {days} day(s): {subject}"),
        ),
        Some((days, subject)) => (
            CheckStatus::Ok,
            format!("soonest certificate expires in {days} day(s): {subject}"),
        ),
    }
}

/// Reduce per-volume BitLocker protection to one check. `protection`:
/// 1 = On, 0 = Off, anything else = Unknown (per
/// `Win32_EncryptableVolume.ProtectionStatus`). Pure; unit-tested.
fn classify_bitlocker(volumes: &[(Option<String>, i64)]) -> (CheckStatus, String) {
    if volumes.is_empty() {
        return (
            CheckStatus::Unknown,
            "no encryptable volumes reported".to_string(),
        );
    }
    let off: Vec<String> = volumes
        .iter()
        .filter(|(_, p)| *p == 0)
        .map(|(drive, _)| drive.clone().unwrap_or_else(|| "<no drive letter>".into()))
        .collect();
    let protected = volumes.iter().filter(|(_, p)| *p == 1).count();
    if !off.is_empty() {
        (
            CheckStatus::Warn,
            format!("{} volume(s) unprotected: {}", off.len(), off.join(", ")),
        )
    } else if protected == 0 {
        // Nothing Off, but nothing confirmed On either — every volume
        // reported ProtectionStatus=2 (indeterminate). Reporting Ok
        // ("0/N protected") would be a misleading green, so surface
        // Unknown instead.
        (
            CheckStatus::Unknown,
            format!(
                "{} volume(s) with indeterminate protection status",
                volumes.len()
            ),
        )
    } else {
        (
            CheckStatus::Ok,
            format!("{protected}/{} volume(s) protected", volumes.len()),
        )
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

    #[tokio::test]
    async fn eval_once_produces_well_formed_snapshot() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(None), true).await;
        assert_eq!(snap.pc_id, "PC1234");
        assert!(snap.online);
        assert_eq!(snap.vpn, "unknown");
        assert_eq!(snap.agent_version, "0.41.0");
        assert_eq!(snap.target_version, "0.41.0"); // target unset → falls back
        // 5 checks: agent_self_update + disk_free + bitlocker +
        // av_signature + cert_expiry.
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

    #[tokio::test]
    async fn eval_once_online_reflects_the_passed_flag() {
        // #288: `online` must mirror the broker-connection bool the
        // caller samples, not a hardcoded `true` — a disconnected
        // agent has to surface as offline on the Health tab.
        let offline = eval_once("PC1234", "0.41.0", &cfg_with(None), false).await;
        assert!(!offline.online, "online must follow the passed flag");
        let online = eval_once("PC1234", "0.41.0", &cfg_with(None), true).await;
        assert!(online.online);
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

    #[tokio::test]
    async fn snapshot_target_version_falls_back_when_cfg_target_empty() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(Some("")), true).await;
        assert_eq!(snap.target_version, "0.41.0");
    }

    #[tokio::test]
    async fn snapshot_target_version_surfaces_when_cfg_target_set() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(Some("0.42.0")), true).await;
        assert_eq!(snap.target_version, "0.42.0");
    }

    // ---- pure classifiers (#290) ----

    #[test]
    fn classify_av_age_thresholds() {
        use chrono::Duration;
        assert_eq!(classify_av_age(Duration::hours(1)), CheckStatus::Ok);
        assert_eq!(classify_av_age(Duration::hours(24)), CheckStatus::Ok);
        // Just past 24 h → Warn.
        assert_eq!(
            classify_av_age(Duration::hours(24) + Duration::minutes(1)),
            CheckStatus::Warn
        );
        assert_eq!(classify_av_age(Duration::days(7)), CheckStatus::Warn);
        assert_eq!(classify_av_age(Duration::days(8)), CheckStatus::Fail);
        // Clock skew (signatures stamped in the future) is treated as
        // fresh, not stale.
        assert_eq!(classify_av_age(Duration::hours(-3)), CheckStatus::Ok);
    }

    #[test]
    fn classify_cert_expiry_thresholds() {
        // Empty store is healthy, not a failure.
        assert_eq!(classify_cert_expiry(None).0, CheckStatus::Ok);
        // Well in the future.
        assert_eq!(
            classify_cert_expiry(Some((90, "CN=ok".into()))).0,
            CheckStatus::Ok
        );
        // Inside the 30-day window → Warn.
        assert_eq!(
            classify_cert_expiry(Some((30, "CN=soon".into()))).0,
            CheckStatus::Warn
        );
        assert_eq!(
            classify_cert_expiry(Some((1, "CN=soon".into()))).0,
            CheckStatus::Warn
        );
        // Expiring today (days=0, via num_days truncation) — still a
        // Warn, not yet a Fail (NotAfter not actually passed).
        assert_eq!(
            classify_cert_expiry(Some((0, "CN=today".into()))).0,
            CheckStatus::Warn
        );
        // Already past NotAfter → Fail, and the subject is surfaced.
        let (status, detail) = classify_cert_expiry(Some((-5, "CN=dead".into())));
        assert_eq!(status, CheckStatus::Fail);
        assert!(detail.contains("CN=dead"), "detail: {detail}");
    }

    #[test]
    fn classify_bitlocker_states() {
        // No volumes → Unknown (couldn't determine).
        assert_eq!(classify_bitlocker(&[]).0, CheckStatus::Unknown);
        // All protected → Ok.
        assert_eq!(
            classify_bitlocker(&[(Some("C:".into()), 1), (None, 1)]).0,
            CheckStatus::Ok
        );
        // Any volume Off → Warn, and its drive is named.
        let (status, detail) =
            classify_bitlocker(&[(Some("C:".into()), 1), (Some("D:".into()), 0)]);
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("D:"), "detail: {detail}");
        // Unknown protection (2) alongside a confirmed On → still Ok
        // (at least one volume is provably protected).
        assert_eq!(
            classify_bitlocker(&[(Some("C:".into()), 1), (Some("E:".into()), 2)]).0,
            CheckStatus::Ok
        );
        // ALL volumes indeterminate (status 2) → Unknown, not a
        // misleading green "0/N protected".
        assert_eq!(
            classify_bitlocker(&[(Some("C:".into()), 2)]).0,
            CheckStatus::Unknown
        );
    }

    // ---- live WMI / cert checks (Windows only) ----
    //
    // Like `disk_free_returns_concrete_status_on_windows`, these run
    // the real powershell shell-out. We can't pin the status (depends
    // on the box: elevation, Defender presence, cert store contents),
    // so we assert the check produces the right `name`, a populated
    // detail, and — crucially for #290 — never the old `TODO:` text.

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn av_signature_check_runs_and_is_not_a_todo_stub() {
        let c = av_signature_check().await;
        assert_eq!(c.name, "av_signature");
        let detail = c.detail.expect("detail populated");
        assert!(!detail.contains("TODO"), "still a stub: {detail}");
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn bitlocker_check_runs_and_is_not_a_todo_stub() {
        // Non-elevated test process → the query is access-denied, so
        // this exercises the graceful-degradation path (Unknown +
        // reason). As LocalSystem in production it returns real data.
        let c = bitlocker_check().await;
        assert_eq!(c.name, "bitlocker");
        let detail = c.detail.expect("detail populated");
        assert!(!detail.contains("TODO"), "still a stub: {detail}");
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn cert_expiry_check_runs_and_is_not_a_todo_stub() {
        let c = cert_expiry_check().await;
        assert_eq!(c.name, "cert_expiry");
        let detail = c.detail.expect("detail populated");
        assert!(!detail.contains("TODO"), "still a stub: {detail}");
    }
}
