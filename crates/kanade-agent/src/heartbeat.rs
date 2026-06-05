use kanade_shared::subject;
use kanade_shared::wire::{EffectiveConfig, Heartbeat};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::watch;
use tracing::{info, warn};

/// `COMPUTERNAME` on Windows, `HOSTNAME` on Unix-likes. Best-effort —
/// the heartbeat baseline is happier with `Some("PC-01")` than with
/// a panic when the env var is missing, so we shrug it off as
/// `None` and the backend backfills via inventory later.
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
}

/// Per-process self-perf snapshot at this heartbeat tick. All `i64`
/// because sysinfo returns u64 but the wire schema (and the SQLite
/// columns the backend persists into) are i64 — clamped to i64::MAX
/// before serialisation so a wildly large value can't wrap.
#[derive(Default)]
struct SelfPerf {
    cpu_pct: Option<f64>,
    rss_bytes: Option<i64>,
    disk_read_bytes: Option<i64>,
    disk_written_bytes: Option<i64>,
}

/// Refresh self-process metrics via sysinfo and return the values
/// we care about.
///
/// On Windows the underlying APIs sysinfo reaches for are
/// `CreateToolhelp32Snapshot` + per-process `GetProcessMemoryInfo` /
/// `GetProcessIoCounters` (NOT WMI / NOT `NtQuerySystemInformation`).
/// Per-tick cost on a typical endpoint with a few hundred processes
/// is single-digit milliseconds; on hosts with thousands of processes
/// (Citrix / RDS) the Toolhelp snapshot itself scales with the
/// process table so the upper bound grows.
///
/// `is_first_tick` lets the caller suppress the always-zero CPU
/// sample sysinfo emits before it has two consecutive measurements
/// to diff: returning `cpu_pct: None` keeps the SPA's "no data yet"
/// dash for the first 30 s after agent restart instead of rendering
/// a misleading "0.0%" that looks like the agent is wedged.
/// `rss_bytes` / `disk_*_bytes` are valid from the first sample so
/// they get populated either way.
///
/// `disk_read_bytes` / `disk_written_bytes` are cumulative bytes
/// since process start. The wire schema serves them as absolute
/// values; clients that want a rate diff successive snapshots
/// themselves (the projector + API currently just store and
/// expose the absolute count).
fn refresh_self_perf(sys: &mut System, is_first_tick: bool) -> SelfPerf {
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage(),
    );
    let Some(proc) = sys.process(pid) else {
        return SelfPerf::default();
    };
    let disk = proc.disk_usage();
    SelfPerf {
        cpu_pct: if is_first_tick {
            None
        } else {
            Some(proc.cpu_usage() as f64)
        },
        rss_bytes: Some(proc.memory().min(i64::MAX as u64) as i64),
        disk_read_bytes: Some(disk.total_read_bytes.min(i64::MAX as u64) as i64),
        disk_written_bytes: Some(disk.total_written_bytes.min(i64::MAX as u64) as i64),
    }
}

/// Heartbeat publisher loop. Cadence is taken from
/// [`EffectiveConfig::heartbeat_duration`] and updates live the
/// moment the config_supervisor pushes a new value on the watch
/// channel.
pub async fn heartbeat_loop(
    client: async_nats::Client,
    pc_id: String,
    agent_version: String,
    mut cfg_rx: watch::Receiver<EffectiveConfig>,
) {
    let mut current_dur = cfg_rx.borrow().heartbeat_duration();
    let mut ticker = tokio::time::interval(current_dur);
    // Read once at startup — neither hostname nor OS family changes
    // over the lifetime of an agent process.
    let hostname = hostname();
    let os_family = Some(std::env::consts::OS.to_string());
    // v0.37 Part 2: long-lived sysinfo System struct so the second-
    // and-subsequent CPU% reads are correctly diffed against the
    // prior sample (sysinfo computes CPU% from cumulative ticks —
    // a fresh System each tick would always report 0). The first
    // tick's CPU% is suppressed via `is_first_tick` so the SPA
    // doesn't show a fake "0.0%" between agent boot and the second
    // heartbeat 30 s later.
    let mut sys = System::new();
    let mut is_first_tick = true;
    info!(
        ?current_dur,
        ?hostname,
        ?os_family,
        "heartbeat loop scheduled",
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let perf = refresh_self_perf(&mut sys, is_first_tick);
                is_first_tick = false;
                let hb = Heartbeat {
                    pc_id: pc_id.clone(),
                    at: chrono::Utc::now(),
                    agent_version: agent_version.clone(),
                    hostname: hostname.clone(),
                    os_family: os_family.clone(),
                    agent_cpu_pct: perf.cpu_pct,
                    agent_rss_bytes: perf.rss_bytes,
                    agent_disk_read_bytes: perf.disk_read_bytes,
                    agent_disk_written_bytes: perf.disk_written_bytes,
                };
                let payload = match serde_json::to_vec(&hb) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, "serialize heartbeat");
                        continue;
                    }
                };
                if let Err(e) = client
                    .publish(subject::heartbeat(&pc_id), payload.into())
                    .await
                {
                    warn!(error = %e, "publish heartbeat");
                }
            }
            res = cfg_rx.changed() => {
                if res.is_err() {
                    // Supervisor dropped: just keep ticking at the
                    // last cadence we knew.
                    continue;
                }
                let new_dur = cfg_rx.borrow().heartbeat_duration();
                if new_dur != current_dur {
                    info!(
                        old = ?current_dur,
                        new = ?new_dur,
                        "heartbeat interval changed; rescheduling ticker",
                    );
                    current_dur = new_dur;
                    ticker = tokio::time::interval(current_dur);
                }
            }
        }
    }
}
