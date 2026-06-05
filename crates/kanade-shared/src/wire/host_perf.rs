//! Host-wide performance snapshot. Phase 1 of the perf telemetry
//! pipeline (the design discussion landed in v0.40).
//!
//! Distinct from the *self*-perf fields already on [`Heartbeat`]:
//! those describe the agent process. `HostPerf` describes the whole
//! machine the agent is running on — CPU / Memory / Disk I/O / Network
//! — and is what powers the per-PC time-series charts in the SPA.
//!
//! Cadence is taken from [`EffectiveConfig::host_perf_interval`]
//! (default 60 s). It's deliberately slower than the 30 s heartbeat
//! because gappy time-series data is acceptable and we'd rather keep
//! the per-host CPU cost ignorable on Citrix / RDS boxes with thousands
//! of processes.
//!
//! All numeric fields are `Option`. The agent populates them when
//! sysinfo succeeds; missing values render as gaps in the chart. The
//! optional shape also keeps forward-compat with future builds that
//! might be unable to read e.g. swap on a sandbox.
//!
//! [`Heartbeat`]: super::Heartbeat
//! [`EffectiveConfig::host_perf_interval`]: super::EffectiveConfig::host_perf_interval

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HostPerf {
    pub pc_id: String,
    pub at: chrono::DateTime<chrono::Utc>,

    /// Whole-host CPU utilisation in percent (0..100), normalised
    /// across all logical cores — the Task Manager "CPU" column
    /// shape, NOT sysinfo's per-core sum (which would report 800 on
    /// an 8-core box pegged at 100 %). On Windows the underlying
    /// signal is `NtQuerySystemInformation(SystemProcessorPerformance
    /// Information)`; cost is sub-millisecond and scales with logical
    /// core count, not process count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    /// Number of logical cores reported by sysinfo. Sent every tick
    /// so the SPA can render "8 cores" alongside the chart without
    /// needing a second API call into the inventory table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,

    /// Physical memory in use, bytes. sysinfo's `System::used_memory()`
    /// — on Windows this is `MEMORYSTATUSEX.ullTotalPhys -
    /// ullAvailPhys`, matching Task Manager's "In use" indicator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_bytes: Option<i64>,
    /// Physical memory total, bytes. Constant for the lifetime of a
    /// host but sent every tick so the SPA can render a ratio without
    /// a second query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_bytes: Option<i64>,
    /// Swap / pagefile used, bytes. `None` on systems with no
    /// configured swap (some sandboxes / containers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_used_bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_total_bytes: Option<i64>,

    /// Disk read throughput across **all** volumes, bytes/sec. The
    /// agent diffs sysinfo's cumulative `Disk::usage()` counters
    /// between successive ticks and divides by the elapsed wall time
    /// — backend stores the rate verbatim. `None` on the first tick
    /// after agent start (no prior sample to diff) and after any
    /// configuration-driven cadence change that resets the baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_read_bytes_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_written_bytes_per_sec: Option<f64>,

    /// Network receive throughput across **all** interfaces (including
    /// loopback), bytes/sec. Same diff-vs-prev-sample shape as
    /// `disk_*_bytes_per_sec`. We include loopback in the total
    /// because filtering it out reliably across Win / Linux is
    /// surprisingly fiddly and most fleet hosts don't drive much
    /// loopback traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_bytes_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_bytes_per_sec: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn host_perf_round_trips_through_json() {
        let s = HostPerf {
            pc_id: "pc-01".into(),
            at: chrono::Utc.with_ymd_and_hms(2026, 5, 24, 0, 0, 0).unwrap(),
            cpu_pct: Some(12.5),
            cpu_count: Some(8),
            mem_used_bytes: Some(8_000_000_000),
            mem_total_bytes: Some(16_000_000_000),
            swap_used_bytes: Some(0),
            swap_total_bytes: Some(4_000_000_000),
            disk_read_bytes_per_sec: Some(1024.0 * 1024.0),
            disk_written_bytes_per_sec: Some(512.0 * 1024.0),
            net_rx_bytes_per_sec: Some(2048.0),
            net_tx_bytes_per_sec: Some(1024.0),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: HostPerf = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pc_id, s.pc_id);
        assert_eq!(back.at, s.at);
        assert_eq!(back.cpu_pct, s.cpu_pct);
        assert_eq!(back.cpu_count, s.cpu_count);
        assert_eq!(back.mem_used_bytes, s.mem_used_bytes);
        assert_eq!(back.mem_total_bytes, s.mem_total_bytes);
        assert_eq!(back.swap_used_bytes, s.swap_used_bytes);
        assert_eq!(back.swap_total_bytes, s.swap_total_bytes);
        assert_eq!(back.disk_read_bytes_per_sec, s.disk_read_bytes_per_sec);
        assert_eq!(
            back.disk_written_bytes_per_sec,
            s.disk_written_bytes_per_sec
        );
        assert_eq!(back.net_rx_bytes_per_sec, s.net_rx_bytes_per_sec);
        assert_eq!(back.net_tx_bytes_per_sec, s.net_tx_bytes_per_sec);
    }

    #[test]
    fn host_perf_with_all_optional_fields_omitted_still_decodes() {
        // Forward-compat: an agent that fails to collect anything
        // beyond pc_id + at still emits a valid HostPerf. The backend
        // projector should keep accepting these and write all-NULL
        // sample rows so the gap is visible in the chart.
        let json = r#"{"pc_id":"x","at":"2026-05-24T00:00:00Z"}"#;
        let s: HostPerf = serde_json::from_str(json).unwrap();
        assert_eq!(s.pc_id, "x");
        assert!(s.cpu_pct.is_none());
        assert!(s.mem_used_bytes.is_none());
        assert!(s.disk_read_bytes_per_sec.is_none());
        assert!(s.net_rx_bytes_per_sec.is_none());
    }
}
