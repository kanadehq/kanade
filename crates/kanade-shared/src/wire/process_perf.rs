//! Per-process snapshot for operator-driven host investigation
//! (v0.41 / Phase 2 of the perf telemetry pipeline).
//!
//! Distinct from [`HostPerf`]: that's a whole-machine roll-up that
//! every agent publishes on a 60 s cadence by default. `ProcessPerf`
//! is the **expensive** path — it requires walking the full OS
//! process table (`CreateToolhelp32Snapshot` on Windows, `/proc`
//! enumeration on Linux), so it's off by default and only turns on
//! per-PC when the operator flips `process_perf_enabled` in
//! `agent_config`. The agent auto-disables publishing the moment
//! `process_perf_expires_at` slides into the past — see
//! [`EffectiveConfig::process_perf_active_at`].
//!
//! [`HostPerf`]: super::HostPerf
//! [`EffectiveConfig::process_perf_active_at`]: super::EffectiveConfig::process_perf_active_at

use serde::{Deserialize, Serialize};

/// One agent tick worth of "top processes by CPU" data. `processes`
/// is already sorted (descending CPU%) and clipped to the
/// `process_perf_top_n` configured for that scope, so the backend
/// projector can persist it verbatim without re-sorting.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessPerf {
    pub pc_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub processes: Vec<ProcessSnapshot>,
}

/// Single process row inside [`ProcessPerf`]. Mirrors the shape of
/// the agent's self-perf fields on [`Heartbeat`] so existing
/// formatters (em-dash for nulls etc.) keep working on the SPA.
///
/// [`Heartbeat`]: super::Heartbeat
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessSnapshot {
    /// PID — `u32` matches Windows `DWORD` and is wide enough for
    /// Linux's 22-bit space too.
    pub pid: u32,
    /// Image name as sysinfo reports it (basename on Linux,
    /// "program.exe" on Windows). NOT the full path — that lives in
    /// inventory if an operator needs to disambiguate two identically
    /// named processes.
    pub name: String,
    /// Percent-of-one-core, same convention as
    /// `Heartbeat::agent_cpu_pct`. A worker pinning two cores reports
    /// `200.0` here; divide by the host's core count for a host-
    /// normalised view.
    pub cpu_pct: f64,
    /// Resident set size in bytes — sysinfo's `Process::memory()`.
    /// `i64` (not `u64`) so the projector binds cleanly via
    /// `sqlx::query!` against an `INTEGER` column.
    pub rss_bytes: i64,
    /// Disk read rate, B/s, computed agent-side by diffing successive
    /// cumulative `Process::disk_usage().total_read_bytes` and
    /// dividing by elapsed wall time. `None` on the very first tick
    /// for a freshly-tracked PID (no prior sample to diff) and after
    /// any cadence change that resets the baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_read_bytes_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_written_bytes_per_sec: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn process_perf_round_trips_through_json() {
        let s = ProcessPerf {
            pc_id: "minipc".into(),
            at: chrono::Utc.with_ymd_and_hms(2026, 5, 24, 1, 0, 0).unwrap(),
            processes: vec![
                ProcessSnapshot {
                    pid: 4321,
                    name: "chrome.exe".into(),
                    cpu_pct: 87.5,
                    rss_bytes: 2_000_000_000,
                    disk_read_bytes_per_sec: Some(1024.0 * 1024.0),
                    disk_written_bytes_per_sec: Some(0.0),
                },
                ProcessSnapshot {
                    pid: 100,
                    name: "systemd".into(),
                    cpu_pct: 0.1,
                    rss_bytes: 50_000_000,
                    disk_read_bytes_per_sec: None,
                    disk_written_bytes_per_sec: None,
                },
            ],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ProcessPerf = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pc_id, s.pc_id);
        assert_eq!(back.at, s.at);
        assert_eq!(back.processes.len(), 2);
        assert_eq!(back.processes[0].pid, 4321);
        assert_eq!(back.processes[0].name, "chrome.exe");
        assert_eq!(back.processes[0].cpu_pct, 87.5);
        assert_eq!(back.processes[1].disk_read_bytes_per_sec, None);
    }

    #[test]
    fn process_snapshot_with_optional_io_omitted_still_decodes() {
        let json = r#"{"pid":1,"name":"init","cpu_pct":0.0,"rss_bytes":1000000}"#;
        let s: ProcessSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(s.pid, 1);
        assert_eq!(s.name, "init");
        assert_eq!(s.disk_read_bytes_per_sec, None);
        assert_eq!(s.disk_written_bytes_per_sec, None);
    }
}
