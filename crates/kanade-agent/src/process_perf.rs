//! Conditional per-process telemetry loop (v0.41 / Phase 2).
//!
//! Off by default. Turns on per-PC the moment an operator flips
//! `process_perf_enabled=true` in `agent_config` (via the SPA toggle
//! or `kanade config set`); turns back off the moment
//! `process_perf_expires_at` slides into the past, even if the flag
//! itself is still `true`. Centralising the active-vs-expired check
//! in [`EffectiveConfig::process_perf_active_at`] keeps agent /
//! backend / SPA in sync on the gate.
//!
//! The loop reuses `host_perf_interval` for cadence — the operator
//! already opted into investigation mode, so collecting at the same
//! rate as the host-wide loop is the natural shape and lets the SPA
//! line up the two charts.
//!
//! Cost: `refresh_processes_specifics(All, ...)` is the most
//! expensive sysinfo call we make — single-digit ms on a typical
//! endpoint, but 100 ms+ on Citrix / RDS hosts with thousands of
//! processes. That cost is why this loop is opt-in and time-boxed.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use kanade_shared::subject;
use kanade_shared::wire::{EffectiveConfig, ProcessPerf, ProcessSnapshot};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Per-PID baseline so successive snapshots can be diffed into a
/// disk-rate-per-second. Without this each sample would show 0
/// bytes/sec because the underlying `total_read_bytes` counter is
/// cumulative since process start — meaningful only as a delta.
#[derive(Clone, Copy)]
struct PidIo {
    cumulative_read: u64,
    cumulative_written: u64,
}

pub async fn process_perf_loop(
    client: async_nats::Client,
    pc_id: String,
    mut cfg_rx: watch::Receiver<EffectiveConfig>,
) {
    // Cadence is taken from `host_perf_interval` (same setting the
    // host-wide perf loop uses) — when an operator opts a host into
    // process-perf investigation, sampling both at the same rate
    // lines up the per-process and whole-host charts.
    let mut current_dur = cfg_rx.borrow().host_perf_duration();
    let mut ticker = tokio::time::interval(current_dur);

    let mut sys = System::new();
    let mut pid_io: HashMap<u32, PidIo> = HashMap::new();
    let mut last_tick_at: Option<Instant> = None;
    // Stays true between the last "published" tick and the next
    // active tick; resets the per-PID disk baseline whenever the
    // active window opens (e.g. after an expiry + re-enable cycle)
    // so a stale baseline from a previous investigation can't
    // produce an absurd first-sample rate.
    let mut needs_baseline_reset = true;

    info!(
        ?current_dur,
        "process_perf loop scheduled (publish gated on agent_config)"
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now_utc = chrono::Utc::now();
                let cfg = cfg_rx.borrow().clone();
                if !cfg.process_perf_active_at(now_utc) {
                    // Idle path. Drop the per-PID baseline so the
                    // next active window starts clean — without
                    // this, a host that flips OFF for hours then
                    // back ON would compute a rate against a stale
                    // cumulative count from the previous run.
                    if !pid_io.is_empty() {
                        debug!("process_perf inactive — clearing PID baseline");
                        pid_io.clear();
                    }
                    last_tick_at = None;
                    needs_baseline_reset = true;
                    continue;
                }

                let now = Instant::now();
                let elapsed = match last_tick_at {
                    Some(prev) => now.duration_since(prev).as_secs_f64(),
                    None => 0.0,
                };
                last_tick_at = Some(now);

                sys.refresh_processes_specifics(
                    ProcessesToUpdate::All,
                    true,
                    ProcessRefreshKind::nothing()
                        .with_cpu()
                        .with_memory()
                        .with_disk_usage(),
                );

                // Gather every process into a sortable Vec with the
                // CPU% read once so the subsequent sort doesn't
                // re-call sysinfo for each comparison.
                let mut all: Vec<(Pid, f32, u64, u64, u64)> = sys
                    .processes()
                    .iter()
                    .map(|(pid, p)| {
                        let d = p.disk_usage();
                        (*pid, p.cpu_usage(), p.memory(), d.total_read_bytes, d.total_written_bytes)
                    })
                    .collect();
                // Descending by CPU%. NaN-safe — sysinfo shouldn't
                // emit NaNs but partial_cmp's `unwrap_or(Equal)`
                // pins them to a stable but unspecified position
                // rather than panicking.
                all.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                });
                let top_n = cfg.process_perf_top_n.max(1) as usize;
                all.truncate(top_n);

                let live_pids: HashSet<u32> = all.iter().map(|(pid, ..)| pid.as_u32()).collect();

                let processes: Vec<ProcessSnapshot> = all
                    .iter()
                    .map(|(pid, cpu, mem, total_read, total_written)| {
                        let pid_u32 = pid.as_u32();
                        let name = sys
                            .process(*pid)
                            .map(|p| p.name().to_string_lossy().into_owned())
                            .unwrap_or_default();

                        let (read_rate, write_rate) = if needs_baseline_reset || elapsed < 0.001 {
                            (None, None)
                        } else {
                            match pid_io.get(&pid_u32) {
                                Some(prev) => {
                                    let d_read = total_read.saturating_sub(prev.cumulative_read);
                                    let d_written =
                                        total_written.saturating_sub(prev.cumulative_written);
                                    (
                                        Some(d_read as f64 / elapsed),
                                        Some(d_written as f64 / elapsed),
                                    )
                                }
                                // PID not previously seen — first
                                // sample for this process in the
                                // current active window. Rates
                                // need a baseline; emit None.
                                None => (None, None),
                            }
                        };
                        pid_io.insert(
                            pid_u32,
                            PidIo {
                                cumulative_read: *total_read,
                                cumulative_written: *total_written,
                            },
                        );

                        ProcessSnapshot {
                            pid: pid_u32,
                            name,
                            cpu_pct: f64::from(*cpu),
                            rss_bytes: (*mem).min(i64::MAX as u64) as i64,
                            disk_read_bytes_per_sec: read_rate,
                            disk_written_bytes_per_sec: write_rate,
                        }
                    })
                    .collect();

                // Bound `pid_io` to PIDs we're still tracking so it
                // doesn't grow unboundedly across hour-long
                // investigation windows on hosts with high process
                // churn.
                pid_io.retain(|pid, _| live_pids.contains(pid));

                needs_baseline_reset = false;

                let snapshot = ProcessPerf {
                    pc_id: pc_id.clone(),
                    at: now_utc,
                    processes,
                };

                let payload = match serde_json::to_vec(&snapshot) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, "serialize ProcessPerf");
                        continue;
                    }
                };
                if let Err(e) = client
                    .publish(subject::process_perf(&pc_id), payload.into())
                    .await
                {
                    warn!(error = %e, "publish process_perf");
                }
            }
            res = cfg_rx.changed() => {
                if res.is_err() {
                    continue;
                }
                let new_dur = cfg_rx.borrow().host_perf_duration();
                if new_dur != current_dur {
                    info!(
                        old = ?current_dur,
                        new = ?new_dur,
                        "process_perf cadence changed (host_perf_interval); rescheduling ticker",
                    );
                    current_dur = new_dur;
                    ticker = tokio::time::interval(current_dur);
                    needs_baseline_reset = true;
                    last_tick_at = None;
                }
            }
        }
    }
}
