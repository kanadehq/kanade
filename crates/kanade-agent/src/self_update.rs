//! Self-update watcher (spec §2.10.5). Sprint 6: target_version
//! arrives via the layered agent_config path now, resolved per-pc /
//! per-group / global by the config_supervisor and pushed on a
//! [`tokio::sync::watch`] channel. Whenever that resolved value
//! drifts from `AGENT_VERSION`, the watcher pulls the new binary
//! from the `agent_releases` Object Store, hashes it (SHA-256),
//! atomically swaps it into the running exe's location, and exits
//! — SCM's failure-actions then restart the service on the new
//! binary.
//!
//! The swap is the cross-volume-safe three-step (copy to `<exe>.new`,
//! rename `<exe>` to `<exe>.old`, rename `.new` to `<exe>`) so the
//! window in which the running exe path holds a partially-written file
//! is zero. Cleanup of `.old` / `.new` from any interrupted attempt
//! happens at startup in `main.rs::cleanup_stale_upgrade_artifacts`.
//!
//! `deploy-agent.ps1` is responsible for configuring `sc.exe failure`
//! and `sc.exe failureflag 1` on the service so SCM treats the
//! self-update exit (code 64) as a recoverable failure and restarts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream;
use kanade_shared::kv::OBJECT_AGENT_RELEASES;
use kanade_shared::wire::EffectiveConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tracing::{info, warn};

/// Persisted across the exit(64) / SCM restart cycle so we can spot a
/// self-update loop: if the agent boots, sees the same `target_version`
/// it just tried to swap to, AND its `running_version` is identical to
/// what was running before the swap (i.e. the swap didn't actually
/// change the embedded `CARGO_PKG_VERSION`), the binary uploaded under
/// that label has a different version baked in than the label claims.
/// Refuse to keep swapping; surface in the log.
///
/// Written under `<data_dir>/last_swap.json`. Discarded once a
/// successful swap (one where `running_version` afterwards matches
/// the target) clears the loop.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct LastSwap {
    target: String,
    running_before: String,
}

pub async fn run(
    client: async_nats::Client,
    running_version: String,
    mut cfg_rx: watch::Receiver<EffectiveConfig>,
    tracker: crate::staleness::Tracker,
) {
    let js = jetstream::new(client.clone());

    // Pre-fix used `match get_object_store ... Err => return;` which
    // permanently killed the self-update subsystem when the bucket
    // wasn't provisioned at boot. Live test found a fleet of agents
    // booted at T0 with no `OBJECT_AGENT_RELEASES` had self-update
    // dead-as-doorknob even after the bucket was provisioned at T1
    // — only an agent restart unstuck them.
    //
    // Retry with backoff. `wait_for_object_store` returns as soon as
    // the bucket is reachable (which the broker reconnect path wakes
    // the tracker for), so recovery is essentially instant once the
    // operator runs `kanade jetstream setup`.
    let store = crate::nats_retry::wait_for_object_store(
        &js,
        &client,
        &tracker,
        OBJECT_AGENT_RELEASES,
        "self_update",
    )
    .await;

    // Boot-time loop check: if last_swap.json says we already tried
    // this target with the same running_version, the binary at that
    // label has a label/version mismatch — refuse to retry.
    let last_swap = read_last_swap();
    if let Some(prev) = &last_swap {
        info!(?prev, "recovered last_swap.json from prior cycle");
    }

    // Initial check against whatever the supervisor's first push
    // (its initial_sync) populated.
    let (mut current_target, jitter) = {
        let cfg = cfg_rx.borrow();
        (
            cfg.target_version.clone(),
            cfg.target_version_jitter_duration(),
        )
    };
    let mut loop_blocked_target: Option<String> = None;
    if let Some(target) = current_target.as_deref()
        && target != running_version
    {
        if is_loop(&last_swap, target, &running_version) {
            loop_blocked_target = Some(target.to_string());
            warn!(
                target,
                running = %running_version,
                "self-update LOOP detected — previous swap to this target produced the same running_version. \
                 Refusing to swap again. The binary under this label has a label/version mismatch; \
                 republish it or clear target_version (`kanade config unset target_version`)."
            );
        } else {
            sleep_jitter(jitter).await;
            if let Err(e) = attempt_swap(&store, target, &running_version).await {
                warn!(error = %e, target, "initial self-update fetch failed");
            }
        }
    } else if last_swap.is_some() {
        // We're past a loop: clear the marker so a future legit
        // rollout to a same-named target isn't falsely blocked.
        clear_last_swap();
    }

    // React to every supervisor push; trigger only when
    // target_version actually changed (cadence-only updates land
    // here too and should be ignored).
    loop {
        if cfg_rx.changed().await.is_err() {
            return;
        }
        let (new_target, jitter) = {
            let cfg = cfg_rx.borrow();
            (
                cfg.target_version.clone(),
                cfg.target_version_jitter_duration(),
            )
        };
        if new_target == current_target {
            continue;
        }
        current_target = new_target.clone();

        // Any target_version change clears a previous loop block —
        // a new operator action means a fresh attempt is in order.
        if loop_blocked_target.is_some() && loop_blocked_target.as_deref() != new_target.as_deref()
        {
            info!("target_version changed; clearing loop block");
            loop_blocked_target = None;
            clear_last_swap();
        }

        if let Some(target) = new_target.as_deref()
            && target != running_version
        {
            if loop_blocked_target.as_deref() == Some(target) {
                warn!(target, "still loop-blocked on this target; ignoring");
                continue;
            }
            sleep_jitter(jitter).await;
            if let Err(e) = attempt_swap(&store, target, &running_version).await {
                warn!(error = %e, target, "self-update fetch failed");
            }
        }
    }
}

fn is_loop(last: &Option<LastSwap>, target: &str, running: &str) -> bool {
    last.as_ref()
        .map(|p| p.target == target && p.running_before == running)
        .unwrap_or(false)
}

fn last_swap_path() -> Option<PathBuf> {
    use kanade_shared::default_paths;
    Some(default_paths::data_dir().join("last_swap.json"))
}

fn read_last_swap() -> Option<LastSwap> {
    let path = last_swap_path()?;
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_last_swap(target: &str, running_before: &str) {
    let Some(path) = last_swap_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = LastSwap {
        target: target.to_string(),
        running_before: running_before.to_string(),
    };
    match serde_json::to_vec(&payload) {
        Ok(b) => {
            if let Err(e) = std::fs::write(&path, b) {
                warn!(error = %e, ?path, "write last_swap.json");
            }
        }
        Err(e) => warn!(error = %e, "encode last_swap.json"),
    }
}

fn clear_last_swap() {
    if let Some(path) = last_swap_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Wrap `maybe_download` with the loop-detection bookkeeping. Records
/// the (target, running_before) tuple before the swap-and-exit so the
/// next boot can check whether the swap actually changed
/// `AGENT_VERSION`.
async fn attempt_swap(
    store: &jetstream::object_store::ObjectStore,
    target: &str,
    running: &str,
) -> Result<()> {
    write_last_swap(target, running);
    maybe_download(store, target, running).await
}

/// Random pause in `0..=max` before the download fires. The point is
/// to de-synchronise a fleet-wide rollout — `kanade agent rollout
/// <v> --global` fans the same KV update out to every agent within
/// milliseconds, and without jitter every agent would hit the Object
/// Store at the same instant. `max == 0` means "fire now" (default
/// for the empty-fleet / dev case and for canary smoke tests).
async fn sleep_jitter(max: Duration) {
    if max.is_zero() {
        return;
    }
    let secs = max.as_secs();
    let pick = if secs == 0 {
        0
    } else {
        use rand::RngExt;
        rand::rng().random_range(0..=secs)
    };
    info!(
        jitter_max_secs = secs,
        sleep_secs = pick,
        "self-update jitter — pausing before download"
    );
    tokio::time::sleep(Duration::from_secs(pick)).await;
}

async fn maybe_download(
    store: &jetstream::object_store::ObjectStore,
    target: &str,
    running: &str,
) -> Result<()> {
    if target == running {
        info!(target, "target_version matches running — no self-update");
        return Ok(());
    }
    info!(
        target,
        running, "target_version drift — downloading new binary"
    );

    let mut object = store
        .get(target)
        .await
        .with_context(|| format!("object store get '{target}'"))?;

    let staging = staging_path(target)?;
    if let Some(parent) = staging.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut file = tokio::fs::File::create(&staging)
        .await
        .with_context(|| format!("create {staging:?}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut object, &mut buf)
            .await
            .context("read object chunk")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .await
            .context("write staged exe")?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    file.flush().await.ok();
    let digest = hasher.finalize();
    info!(
        target,
        path = ?staging,
        bytes = total,
        sha256 = %hex(&digest),
        "staged new agent binary — beginning atomic swap",
    );

    swap_and_restart(&staging, target).await?;
    // Unreachable: swap_and_restart calls std::process::exit on success.
    Ok(())
}

/// Replace the running exe with the staged one and exit so SCM's
/// failure-actions can restart the service on the new binary.
///
/// Sequence (cross-volume safe: staging is under `%ProgramData%`,
/// the running exe under `%ProgramFiles%`):
///   1. Copy `<staged>` to `<exe>.new` in the exe's directory.
///   2. Rename `<exe>` to `<exe>.old`. Allowed even though the file
///      is mapped — Windows blocks delete-while-loaded, not rename.
///   3. Rename `<exe>.new` to `<exe>` — atomic within the same dir.
///   4. `std::process::exit(64)`. With `sc.exe failureflag <svc> 1`
///      configured on the service, SCM treats this as a recoverable
///      failure and applies the configured restart action.
///
/// Startup-time cleanup of `<exe>.old` lives in `main.rs` so the
/// stale binary doesn't accumulate.
async fn swap_and_restart(staged: &Path, target_version: &str) -> Result<()> {
    let current = std::env::current_exe().context("current_exe")?;
    let exe_dir = current
        .parent()
        .context("current_exe has no parent directory")?;
    let exe_name = current
        .file_name()
        .and_then(|s| s.to_str())
        .context("current_exe has no UTF-8 file name")?
        .to_string();
    let new_path = exe_dir.join(format!("{exe_name}.new"));
    let old_path = exe_dir.join(format!("{exe_name}.old"));

    // Tidy any leftover .new / .old from a previous interrupted run
    // so the renames below always have a clean target.
    let _ = tokio::fs::remove_file(&new_path).await;
    let _ = tokio::fs::remove_file(&old_path).await;

    tokio::fs::copy(staged, &new_path)
        .await
        .with_context(|| format!("copy {staged:?} -> {new_path:?}"))?;

    tokio::fs::rename(&current, &old_path)
        .await
        .with_context(|| format!("rename {current:?} -> {old_path:?}"))?;
    tokio::fs::rename(&new_path, &current)
        .await
        .with_context(|| format!("rename {new_path:?} -> {current:?}"))?;

    info!(
        target = target_version,
        replaced = ?current,
        backup   = ?old_path,
        "swap complete — exiting (code 64); SCM failure-actions take over",
    );

    // Let the tracing subscriber flush its buffer before SCM kills us.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    std::process::exit(64);
}

fn staging_path(version: &str) -> Result<PathBuf> {
    use kanade_shared::default_paths;
    let exe = std::env::current_exe().context("current_exe")?;
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("kanade-agent")
        .to_string();
    // Spec §2.11.3 — staged binaries live in the data dir, never next
    // to the running exe (Program Files is read-only for LocalSystem
    // services after MSI install).
    Ok(default_paths::data_dir()
        .join("staging")
        .join(format!("{stem}.{version}.staged")))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
