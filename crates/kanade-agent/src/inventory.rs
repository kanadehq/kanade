//! Hardware inventory loop + on-demand collection.
//!
//! v0.12.0 pivot: collection is delegated to `powershell.exe
//! -NoProfile -Command "..."` instead of the `wmi` crate's direct
//! WMI calls. The wmi crate hits `WBEM_E_INVALID_CLASS (0x80041010)`
//! on at least one host's LocalSystem context — same machine where
//! Get-CimInstance from a user-context shell returns the expected
//! data fine. Shelling out to PowerShell is also the design
//! direction for v0.13.0 (operator-defined inventory probes), so
//! this is a step on that path rather than a workaround we'll throw
//! away.
//!
//! The shape returned by the PS snippet matches `HwInventory` JSON
//! so the wire-side projector + UI keep working untouched.

use kanade_shared::wire::EffectiveConfig;
use tokio::sync::watch;

/// Top-level inventory loop. Cadence + jitter + on/off all come from
/// the supervisor's [`EffectiveConfig`] and update live.
///
/// Platform split: the PowerShell snippet only makes sense on
/// Windows. Non-Windows builds keep a no-op shim so CI on Linux /
/// macOS still compiles the agent crate.
pub async fn inventory_loop(
    client: async_nats::Client,
    pc_id: String,
    cfg_rx: watch::Receiver<EffectiveConfig>,
) {
    inner::run(client, pc_id, cfg_rx).await;
}

/// On-demand inventory request handler. Subscribes to
/// `request.inventory.<pc_id>`; on each message, collects WMI
/// inventory once, publishes it to `inventory.<pc_id>.hw` (the same
/// subject the cadence loop uses), and replies to the caller with
/// `"ok"` or `"error: <chain>"`.
pub async fn serve_requests(client: async_nats::Client, pc_id: String) {
    inner::serve_requests(client, pc_id).await;
}

#[cfg(target_os = "windows")]
mod inner {
    use anyhow::{Context, Result, anyhow};
    use futures::StreamExt;
    use kanade_shared::subject;
    use kanade_shared::wire::{EffectiveConfig, HwInventory};
    use tokio::process::Command;
    use tokio::sync::watch;
    use tracing::{info, warn};

    use super::random_jitter;

    /// PowerShell snippet that emits one `HwInventory` JSON object
    /// on stdout. The shape matches `kanade_shared::wire::HwInventory`
    /// so the agent can `serde_json::from_slice::<HwInventory>` it
    /// directly.
    ///
    /// `Get-CimInstance` (modern WMI bridge) works in user + service
    /// contexts where the raw `wmi` crate path stumbled. `ConvertTo-
    /// Json -Compress -Depth 5` keeps the output small enough to
    /// fit in a single NATS message without losing nested disk
    /// data.
    const PS_INVENTORY: &str = r#"
$ErrorActionPreference = 'Stop'
$cs    = Get-CimInstance Win32_ComputerSystem
$os    = Get-CimInstance Win32_OperatingSystem
$cpus  = @(Get-CimInstance Win32_Processor)
$cpu   = $cpus | Select-Object -First 1
$disks = @(Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' |
    ForEach-Object {
        [pscustomobject]@{
            device_id   = $_.DeviceID
            size_bytes  = [int64]($_.Size      | ForEach-Object { if ($_ -eq $null) { 0 } else { $_ } })
            free_bytes  = [int64]($_.FreeSpace | ForEach-Object { if ($_ -eq $null) { 0 } else { $_ } })
            file_system = $_.FileSystem
        }
    })
[pscustomobject]@{
    pc_id        = $env:KANADE_PC_ID
    hostname     = $cs.Name
    os_name      = $os.Caption
    os_version   = $os.Version
    os_build     = $os.BuildNumber
    cpu_model    = $cpu.Name
    cpu_cores    = ($cpus | Measure-Object -Property NumberOfCores -Sum).Sum
    ram_bytes    = [int64]$cs.TotalPhysicalMemory
    disks        = $disks
    collected_at = (Get-Date).ToUniversalTime().ToString('o')
} | ConvertTo-Json -Compress -Depth 5
"#;

    pub async fn run(
        client: async_nats::Client,
        pc_id: String,
        mut cfg_rx: watch::Receiver<EffectiveConfig>,
    ) {
        // No initial pause: dev / fresh-deploy UX suffers when the
        // agent goes invisible to /api/agents for up to
        // `inventory_jitter` (default 10m) after startup. WMI runs
        // per-host so there's no cross-fleet contention to spread out,
        // and the inventory subject is 1-2 KB / agent so a 3000-host
        // simultaneous burst is < 6 MB on the broker — well within
        // JetStream's headroom. The jitter still applies to every
        // subsequent cycle, which is where the "don't all phone home
        // at exactly the same moment 24h later" benefit actually
        // matters.
        loop {
            // Snapshot the current effective config once per cycle —
            // changes that arrive mid-PowerShell-spawn take effect on
            // the next iteration, which is fine.
            let snapshot = cfg_rx.borrow().clone();

            if !snapshot.inventory_enabled {
                info!("inventory collection disabled; waiting for config update");
                if cfg_rx.changed().await.is_err() {
                    return;
                }
                continue;
            }

            match collect_and_publish_once(&client, &pc_id).await {
                Ok(()) => {}
                Err(e) => warn!(error = ?e, "collect+publish hw inventory"),
            }

            let wait = snapshot.inventory_interval_duration()
                + random_jitter(snapshot.inventory_jitter_duration());

            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                res = cfg_rx.changed() => {
                    if res.is_err() {
                        tokio::time::sleep(snapshot.inventory_interval_duration()).await;
                    } else {
                        info!("inventory config changed; re-evaluating");
                    }
                }
            }
        }
    }

    pub async fn serve_requests(client: async_nats::Client, pc_id: String) {
        let subj = subject::inventory_request(&pc_id);
        let mut sub = match client.subscribe(subj.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = ?e, subject = %subj, "subscribe request.inventory failed");
                return;
            }
        };
        info!(subject = %subj, "request.inventory handler ready");

        while let Some(msg) = sub.next().await {
            let reply = msg.reply.clone();
            let client = client.clone();
            let pc = pc_id.clone();
            tokio::spawn(async move {
                let body = match collect_and_publish_once(&client, &pc).await {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("error: {e:#}"),
                };
                if let Some(reply_to) = reply {
                    if let Err(e) = client.publish(reply_to, body.clone().into()).await {
                        warn!(error = ?e, "publish request.inventory reply");
                    }
                }
                if body != "ok" {
                    warn!(reply = %body, "request.inventory collection failed");
                }
            });
        }
    }

    /// Run the PS snippet, parse the stdout JSON into HwInventory,
    /// stamp the local `collected_at`, publish to inventory subject.
    async fn collect_and_publish_once(client: &async_nats::Client, pc_id: &str) -> Result<()> {
        let snap = collect_hw(pc_id)
            .await
            .context("collect hw via PowerShell")?;
        publish(client, &snap).await
    }

    /// Spawn `powershell.exe -NoProfile -Command <PS_INVENTORY>` and
    /// parse its stdout into `HwInventory`.
    async fn collect_hw(pc_id: &str) -> Result<HwInventory> {
        let out = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", PS_INVENTORY])
            .env("KANADE_PC_ID", pc_id)
            .output()
            .await
            .context("spawn powershell.exe for inventory")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "powershell exited {} (stderr: {})",
                out.status,
                stderr.trim()
            ));
        }
        let stdout = std::str::from_utf8(&out.stdout)
            .context("powershell stdout is not UTF-8")?
            .trim();
        if stdout.is_empty() {
            return Err(anyhow!("powershell stdout was empty"));
        }
        let snap: HwInventory = serde_json::from_str(stdout)
            .with_context(|| format!("decode HwInventory from powershell stdout: {stdout}"))?;
        Ok(snap)
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

#[cfg(target_os = "windows")]
fn random_jitter(max: std::time::Duration) -> std::time::Duration {
    use rand::Rng;
    let secs = max.as_secs().max(1);
    let r = rand::rng().random_range(0..secs);
    std::time::Duration::from_secs(r)
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

    pub async fn serve_requests(_client: async_nats::Client, _pc_id: String) {
        tracing::info!("request.inventory handler skipped (non-Windows platform)");
    }
}
