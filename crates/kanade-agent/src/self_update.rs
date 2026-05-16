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

use anyhow::{Context, Result};
use async_nats::jetstream;
use kanade_shared::kv::OBJECT_AGENT_RELEASES;
use kanade_shared::wire::EffectiveConfig;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tracing::{info, warn};

pub async fn run(
    client: async_nats::Client,
    running_version: String,
    mut cfg_rx: watch::Receiver<EffectiveConfig>,
) {
    let js = jetstream::new(client);

    let store = match js.get_object_store(OBJECT_AGENT_RELEASES).await {
        Ok(s) => s,
        Err(_) => {
            info!(
                store = OBJECT_AGENT_RELEASES,
                "agent_releases Object Store missing — self-update idle"
            );
            return;
        }
    };

    // Initial check against whatever the supervisor's first push
    // (its initial_sync) populated.
    let mut current_target = cfg_rx.borrow().target_version.clone();
    if let Some(target) = current_target.as_deref()
        && target != running_version
    {
        if let Err(e) = maybe_download(&store, target, &running_version).await {
            warn!(error = %e, target, "initial self-update fetch failed");
        }
    }

    // React to every supervisor push; trigger only when
    // target_version actually changed (cadence-only updates land
    // here too and should be ignored).
    loop {
        if cfg_rx.changed().await.is_err() {
            return;
        }
        let new_target = cfg_rx.borrow().target_version.clone();
        if new_target == current_target {
            continue;
        }
        current_target = new_target.clone();
        if let Some(target) = new_target.as_deref()
            && target != running_version
        {
            if let Err(e) = maybe_download(&store, target, &running_version).await {
                warn!(error = %e, target, "self-update fetch failed");
            }
        }
    }
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
