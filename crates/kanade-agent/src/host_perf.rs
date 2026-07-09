//! Host-wide perf telemetry loop (v0.40 Part 1).
//!
//! Distinct from `heartbeat::heartbeat_loop` — the heartbeat reports
//! the agent's *self* perf (cpu/rss/disk-io of the agent process); this
//! loop reports the *whole machine* (cpu/mem/disk/net of every process
//! on the host). The two loops run in parallel on independent
//! cadences (heartbeat default 30 s, host_perf default 60 s) so the
//! slightly heavier host-wide sysinfo refresh stays out of the tight
//! liveness path.
//!
//! Cadence is taken from [`EffectiveConfig::host_perf_duration`] and
//! reacts live to config_supervisor pushes — same shape as heartbeat.

use std::time::Instant;

use kanade_shared::subject;
use kanade_shared::wire::{EffectiveConfig, HostPerf};
use sysinfo::{Disks, Networks, System};
use tokio::sync::watch;
use tracing::{info, warn};

/// Host-wide perf publisher loop. Cadence comes from
/// `EffectiveConfig::host_perf_interval`. On Windows the sysinfo
/// calls underneath are:
///   * `NtQuerySystemInformation(SystemProcessorPerformanceInformation)`
///     for the CPU mean,
///   * `GlobalMemoryStatusEx` for memory + swap totals,
///   * `GetDiskFreeSpaceEx` + per-volume I/O counters for disks,
///   * `GetIfTable2` for network throughput.
///
/// Aggregate per-host cost on a typical endpoint is single-digit ms —
/// well within the budget for a 60 s loop. On Citrix / RDS hosts with
/// thousands of processes the host-wide refresh is still cheap because
/// none of these APIs walk the process table.
pub async fn host_perf_loop(
    client: async_nats::Client,
    pc_id: String,
    mut cfg_rx: watch::Receiver<EffectiveConfig>,
) {
    let mut current_dur = cfg_rx.borrow().host_perf_duration();
    let mut ticker = tokio::time::interval(current_dur);

    // Long-lived sysinfo handles so successive refreshes give us
    // deltas (CPU% needs two samples; Disks::usage().read_bytes and
    // NetworkData::received() are "since last refresh"). A fresh
    // handle each tick would always report a near-zero or garbage
    // delta.
    let mut sys = System::new();
    let mut disks = Disks::new_with_refreshed_list();
    let mut networks = Networks::new_with_refreshed_list();
    // Wall time at the previous tick — used to convert the
    // refresh-delta byte counts into a per-second rate. Initialised
    // to "now" so the very first tick has a sane (but unused)
    // baseline; we still suppress the first-tick rates via
    // `is_first_tick` because the delta straddles agent-startup
    // overhead and won't reflect steady-state behaviour.
    let mut last_tick_at = Instant::now();
    // Suppresses cpu_pct + disk/net rates on the first tick: sysinfo's
    // CPU% needs two consecutive samples to diff, and the disk/net
    // "delta since last refresh" on the first tick straddles agent
    // startup, so reporting either would mislead the chart.
    let mut is_first_tick = true;

    info!(?current_dur, "host_perf loop scheduled");

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = Instant::now();
                let elapsed = now.duration_since(last_tick_at).as_secs_f64();
                last_tick_at = now;

                sys.refresh_cpu_all();
                sys.refresh_memory();
                // `false` = don't drop entries that disappeared since
                // the last refresh; we'd rather keep summing the
                // last-known value than have a removable disk vanish
                // mid-sample and skew the total downward.
                disks.refresh(false);
                networks.refresh(false);

                let cpu_pct = if is_first_tick {
                    None
                } else {
                    // sysinfo's global_cpu_usage() returns the
                    // arithmetic mean over logical cores as f32
                    // percent (0..100) — Task Manager shape, NOT
                    // sysinfo's per-process convention of
                    // percent-of-one-core.
                    Some(f64::from(sys.global_cpu_usage()))
                };
                // #418 constraints.require.cpu_below reuses this sample —
                // publish it to the env gate (None on the first tick =
                // "unknown" until two samples exist). global_cpu_usage()
                // is f32, so the down-cast is exact.
                crate::env_gate::set_system_cpu(cpu_pct.map(|p| p as f32));
                let cpu_count = u32::try_from(sys.cpus().len()).ok();

                let mem_used = i64::try_from(sys.used_memory()).ok();
                let mem_total = i64::try_from(sys.total_memory()).ok();
                let swap_used = i64::try_from(sys.used_swap()).ok();
                let swap_total = i64::try_from(sys.total_swap()).ok();

                let (disk_read_rate, disk_write_rate) = if is_first_tick || elapsed < 0.001 {
                    (None, None)
                } else {
                    let mut read_bytes: u64 = 0;
                    let mut write_bytes: u64 = 0;
                    for disk in disks.iter() {
                        let usage = disk.usage();
                        read_bytes = read_bytes.saturating_add(usage.read_bytes);
                        write_bytes = write_bytes.saturating_add(usage.written_bytes);
                    }
                    (
                        Some(read_bytes as f64 / elapsed),
                        Some(write_bytes as f64 / elapsed),
                    )
                };

                let (net_rx_rate, net_tx_rate) = if is_first_tick || elapsed < 0.001 {
                    (None, None)
                } else {
                    let mut rx_bytes: u64 = 0;
                    let mut tx_bytes: u64 = 0;
                    for data in networks.values() {
                        rx_bytes = rx_bytes.saturating_add(data.received());
                        tx_bytes = tx_bytes.saturating_add(data.transmitted());
                    }
                    (
                        Some(rx_bytes as f64 / elapsed),
                        Some(tx_bytes as f64 / elapsed),
                    )
                };

                is_first_tick = false;

                let snapshot = HostPerf {
                    pc_id: pc_id.clone(),
                    at: chrono::Utc::now(),
                    cpu_pct,
                    cpu_count,
                    mem_used_bytes: mem_used,
                    mem_total_bytes: mem_total,
                    swap_used_bytes: swap_used,
                    swap_total_bytes: swap_total,
                    disk_read_bytes_per_sec: disk_read_rate,
                    disk_written_bytes_per_sec: disk_write_rate,
                    net_rx_bytes_per_sec: net_rx_rate,
                    net_tx_bytes_per_sec: net_tx_rate,
                };

                let payload = match serde_json::to_vec(&snapshot) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, "serialize HostPerf");
                        continue;
                    }
                };
                if let Err(e) = client
                    .publish(subject::host_perf(&pc_id), payload.into())
                    .await
                {
                    warn!(error = %e, "publish host_perf");
                }
            }
            res = cfg_rx.changed() => {
                if res.is_err() {
                    // Supervisor dropped: keep ticking at the last
                    // cadence we knew, same policy as heartbeat.
                    continue;
                }
                let new_dur = cfg_rx.borrow().host_perf_duration();
                if new_dur != current_dur {
                    info!(
                        old = ?current_dur,
                        new = ?new_dur,
                        "host_perf interval changed; rescheduling ticker",
                    );
                    current_dur = new_dur;
                    ticker = tokio::time::interval(current_dur);
                    // The next tick fires immediately when interval()
                    // is rebuilt, so its delta-vs-last-tick would
                    // straddle the old + new cadence. Reset the
                    // first-tick guard to drop the first rate
                    // computation after the cadence change.
                    is_first_tick = true;
                    last_tick_at = Instant::now();
                }
            }
        }
    }
}
