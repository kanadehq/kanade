#[cfg(target_os = "windows")]
use std::time::Duration;

use kanade_shared::wire::EffectiveConfig;
use tokio::sync::watch;

/// Top-level inventory loop. Cadence + jitter + on/off all come from
/// the supervisor's [`EffectiveConfig`] and update live; nothing here
/// is read once at startup.
///
/// Platform split: WMI collection lives in the `inner` module under
/// `cfg(target_os = "windows")`; the non-Windows build is a no-op
/// shim so the rest of the agent compiles for CI clippy on Linux.
pub async fn inventory_loop(
    client: async_nats::Client,
    pc_id: String,
    cfg_rx: watch::Receiver<EffectiveConfig>,
) {
    inner::run(client, pc_id, cfg_rx).await;
}

#[cfg(target_os = "windows")]
fn random_jitter(max: Duration) -> Duration {
    use rand::Rng;
    let secs = max.as_secs().max(1);
    let r = rand::rng().random_range(0..secs);
    Duration::from_secs(r)
}

#[cfg(target_os = "windows")]
mod inner {
    use std::time::Duration;

    use anyhow::{Context, Result};
    use kanade_shared::subject;
    use kanade_shared::wire::{DiskInfo, EffectiveConfig, HwInventory};
    use serde::Deserialize;
    use tokio::sync::watch;
    use tracing::{info, warn};
    use wmi::WMIConnection;

    use super::random_jitter;

    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Win32ComputerSystem {
        name: String,
        total_physical_memory: u64,
    }

    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Win32OperatingSystem {
        caption: String,
        version: String,
        build_number: String,
    }

    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Win32Processor {
        name: String,
        number_of_cores: u32,
    }

    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Win32LogicalDisk {
        #[serde(rename = "DeviceID")]
        device_id: String,
        size: Option<u64>,
        free_space: Option<u64>,
        file_system: Option<String>,
    }

    pub async fn run(
        client: async_nats::Client,
        pc_id: String,
        mut cfg_rx: watch::Receiver<EffectiveConfig>,
    ) {
        // Initial random pause within the configured jitter so a
        // freshly-restarted fleet doesn't all hit WMI in unison.
        let init_jitter = cfg_rx.borrow().inventory_jitter_duration();
        let init_pause = random_jitter(init_jitter);
        info!(?init_pause, "inventory loop initial jitter");
        tokio::time::sleep(init_pause).await;

        loop {
            // Snapshot the current effective config once per cycle —
            // changes that arrive mid-WMI-query take effect on the
            // next iteration, which is fine.
            let snapshot = cfg_rx.borrow().clone();

            if !snapshot.inventory_enabled {
                info!("inventory collection disabled; waiting for config update");
                // Block until the supervisor pushes a new value;
                // exit if the supervisor went away.
                if cfg_rx.changed().await.is_err() {
                    return;
                }
                continue;
            }

            let pc = pc_id.clone();
            match tokio::task::spawn_blocking(move || collect_hw(&pc)).await {
                Ok(Ok(snap)) => {
                    if let Err(e) = publish(&client, &snap).await {
                        warn!(error = %e, "publish hw inventory");
                    }
                }
                Ok(Err(e)) => warn!(error = %e, "collect hw inventory"),
                Err(e) => warn!(error = %e, "inventory worker join"),
            }

            let wait = snapshot.inventory_interval_duration()
                + random_jitter(snapshot.inventory_jitter_duration());

            // Sleep until next cycle, but wake early if config
            // changes — operator may have just disabled / shortened
            // the interval.
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                res = cfg_rx.changed() => {
                    if res.is_err() {
                        // Supervisor died — keep our last snapshot
                        // and continue at the planned cadence.
                        tokio::time::sleep(wait_ish(snapshot.inventory_interval_duration())).await;
                    } else {
                        info!("inventory config changed; re-evaluating");
                    }
                }
            }
        }
    }

    /// Tiny wrapper to keep the sleep call inside the select arm
    /// readable when the supervisor channel hangs up. Behaviour-wise
    /// identical to sleeping for the configured interval again.
    fn wait_ish(d: Duration) -> Duration {
        d
    }

    fn collect_hw(pc_id: &str) -> Result<HwInventory> {
        let wmi = WMIConnection::new().context("WMI connection")?;

        let cs: Vec<Win32ComputerSystem> = wmi.query().context("Win32_ComputerSystem")?;
        let os_rows: Vec<Win32OperatingSystem> = wmi.query().context("Win32_OperatingSystem")?;
        let cpu_rows: Vec<Win32Processor> = wmi.query().context("Win32_Processor")?;
        let disk_rows: Vec<Win32LogicalDisk> = wmi
            .raw_query("SELECT * FROM Win32_LogicalDisk WHERE DriveType = 3")
            .context("Win32_LogicalDisk")?;

        let cs_first = cs
            .into_iter()
            .next()
            .context("Win32_ComputerSystem empty")?;
        let os_first = os_rows
            .into_iter()
            .next()
            .context("Win32_OperatingSystem empty")?;
        let cpu_cores: u32 = cpu_rows.iter().map(|c| c.number_of_cores).sum();
        let cpu_first = cpu_rows
            .into_iter()
            .next()
            .context("Win32_Processor empty")?;

        let disks: Vec<DiskInfo> = disk_rows
            .into_iter()
            .map(|d| DiskInfo {
                device_id: d.device_id,
                size_bytes: d.size.unwrap_or(0),
                free_bytes: d.free_space.unwrap_or(0),
                file_system: d.file_system,
            })
            .collect();

        Ok(HwInventory {
            pc_id: pc_id.to_string(),
            hostname: cs_first.name,
            os_name: os_first.caption,
            os_version: os_first.version,
            os_build: Some(os_first.build_number),
            cpu_model: cpu_first.name,
            cpu_cores,
            ram_bytes: cs_first.total_physical_memory,
            disks,
            collected_at: chrono::Utc::now(),
        })
    }

    async fn publish(client: &async_nats::Client, snapshot: &HwInventory) -> Result<()> {
        let payload = serde_json::to_vec(snapshot)?;
        let subj = subject::inventory(&snapshot.pc_id, subject::INVENTORY_HW);
        client.publish(subj, payload.into()).await?;
        info!(
            pc_id = %snapshot.pc_id,
            disks = snapshot.disks.len(),
            ram_gb = snapshot.ram_bytes / (1024 * 1024 * 1024),
            "published hw inventory",
        );
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
mod inner {
    use kanade_shared::wire::EffectiveConfig;
    use tokio::sync::watch;

    pub async fn run(
        _client: async_nats::Client,
        _pc_id: String,
        _cfg_rx: watch::Receiver<EffectiveConfig>,
    ) {
        tracing::info!("inventory collection skipped (non-Windows platform)");
    }
}
