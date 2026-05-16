#[cfg(target_os = "windows")]
use std::time::Duration;

use kanade_shared::config::InventorySection;
#[cfg(target_os = "windows")]
use tracing::warn;

/// Top-level inventory loop. Delegates to a Windows-only WMI collector or
/// a non-Windows stub depending on the build target.
pub async fn inventory_loop(client: async_nats::Client, pc_id: String, cfg: InventorySection) {
    if !cfg.enabled {
        tracing::info!("inventory disabled in config");
        return;
    }
    inner::run(client, pc_id, cfg).await;
}

// Helpers used by the Windows inner module only — gated so Linux /
// macOS clippy doesn't flag them as dead code.
#[cfg(target_os = "windows")]
fn parse_or_default(label: &str, value: &str, fallback: Duration) -> Duration {
    match humantime::parse_duration(value) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, label, value, "invalid duration, using fallback");
            fallback
        }
    }
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
    use kanade_shared::config::InventorySection;
    use kanade_shared::subject;
    use kanade_shared::wire::{DiskInfo, HwInventory};
    use serde::Deserialize;
    use tracing::{info, warn};
    use wmi::WMIConnection;

    use super::{parse_or_default, random_jitter};

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

    pub async fn run(client: async_nats::Client, pc_id: String, cfg: InventorySection) {
        let interval = parse_or_default(
            "hw_interval",
            &cfg.hw_interval,
            Duration::from_secs(24 * 3600),
        );
        let jitter = parse_or_default("jitter", &cfg.jitter, Duration::from_secs(600));

        // Initial random pause within `jitter` so a freshly-restarted
        // fleet doesn't all hit WMI at once.
        let init_pause = random_jitter(jitter);
        info!(?interval, ?jitter, ?init_pause, "inventory loop scheduled");
        tokio::time::sleep(init_pause).await;

        loop {
            let pc = pc_id.clone();
            match tokio::task::spawn_blocking(move || collect_hw(&pc)).await {
                Ok(Ok(snapshot)) => {
                    if let Err(e) = publish(&client, &snapshot).await {
                        warn!(error = %e, "publish hw inventory");
                    }
                }
                Ok(Err(e)) => warn!(error = %e, "collect hw inventory"),
                Err(e) => warn!(error = %e, "inventory worker join"),
            }
            let wait = interval + random_jitter(jitter);
            tokio::time::sleep(wait).await;
        }
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
    use kanade_shared::config::InventorySection;

    pub async fn run(_client: async_nats::Client, _pc_id: String, _cfg: InventorySection) {
        tracing::info!("inventory collection skipped (non-Windows platform)");
    }
}
