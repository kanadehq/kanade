use serde::{Deserialize, Serialize};

/// Liveness ping every agent sends on a 30 s cadence (see
/// `inventory_interval` / `heartbeat_interval` in agent_config).
///
/// `hostname` and `os_family` are enriched baseline facts so the
/// SPA agents page has *something* to show as soon as the agent
/// boots — even when the full WMI-driven `HwInventory` hasn't been
/// (or can't be) collected. Both stay `Option<String>` so older
/// agents that don't send them still deserialize cleanly.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Heartbeat {
    pub pc_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub agent_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Coarse OS bucket from `std::env::consts::OS` — `"windows"`,
    /// `"linux"`, `"macos"`. Rich OS metadata still flows through
    /// the inventory path; this is just the "agent is alive on a
    /// <family>" signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_family: Option<String>,
    // v0.37 / Part 2: agent process self-perf. All Option so older
    // agents (or any future build that hits a sysinfo error) keep
    // sending valid heartbeats — backend just shows blanks. Cost on
    // the agent is one `sysinfo::System::refresh_processes_specifics`
    // call per 30 s tick. On Windows the underlying APIs are
    // `CreateToolhelp32Snapshot` + per-process `GetProcessMemoryInfo`
    // / `GetProcessIoCounters` (NOT WMI; NOT
    // `NtQuerySystemInformation`). Single-digit ms on a typical
    // endpoint; scales with the host's process count for the
    // Toolhelp snapshot — fine on a normal PC, larger on RDS hosts.
    /// Agent process CPU usage, in percent-of-one-core (a process
    /// fully pinning one core reports 100; one pinning two cores
    /// reports 200). This is sysinfo's convention — closer to
    /// `top` than to Windows Task Manager (which normalises by
    /// total cores, so a 1-core peg on an 8-core box shows up as
    /// ~12.5 % in TM). Divide by host core count if you want a
    /// host-normalised view. `None` is published on the very first
    /// heartbeat after process start, because sysinfo's CPU% needs
    /// two consecutive samples to diff — populating it would
    /// always report 0.0 there and risk an operator misreading
    /// "agent isn't doing anything".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_cpu_pct: Option<f64>,
    /// Agent process resident set size in bytes — sysinfo's
    /// `Process::memory()`, which on Windows is
    /// `PROCESS_MEMORY_COUNTERS_EX::WorkingSetSize` (full working
    /// set, shared + private). Closest Task Manager column is
    /// "Working set (memory)", NOT "Memory (private working set)"
    /// which would be `PrivateUsage` and sysinfo exposes
    /// separately as `virtual_memory()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_rss_bytes: Option<i64>,
    /// Absolute bytes the agent process has read from disk since
    /// it started. Wire format is cumulative (not delta) so
    /// dropped / out-of-order heartbeats don't poison rate math
    /// for any client that wants to derive a rate by diffing
    /// successive snapshots. Today neither the backend projector
    /// nor the SPA does that diff — they just store and render
    /// the cumulative value. Future SPA work or an exporter can
    /// compute rate without a schema change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_disk_read_bytes: Option<i64>,
    /// Absolute bytes the agent process has written to disk since
    /// it started. Same shape as `agent_disk_read_bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_disk_written_bytes: Option<i64>,
    /// #582 Phase 2: versions this agent's boot sentinel rolled back
    /// after they crash-looped on boot. The self-update path refuses
    /// to (re-)deploy any version listed here, so the SPA's rollout
    /// view can flag "PC-X failed to adopt target 0.43.51" — the
    /// fleet-wide signal that a rollout is bad. Empty (the common
    /// case) is skipped on the wire; older agents simply omit it and
    /// `#[serde(default)]` leaves it empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined_versions: Vec<String>,
    /// Most-recently signed-in account on this host, read from the
    /// Windows `LogonUI` registry key
    /// (`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\LogonUI\LastLoggedOnUser`).
    /// This is the `DOMAIN\sam` (or `.\user`) login name the sign-in
    /// screen last used; it survives logoff, so it's populated even
    /// when no one is currently signed in. `None` on a never-signed-in
    /// host and on non-Windows agents (`read_hklm_value` returns `None`
    /// off-Windows) — see #655 for the cross-platform follow-up — so
    /// older agents keep sending valid heartbeats either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_logon_user: Option<String>,
    /// Display name paired with [`Self::last_logon_user`], from
    /// `LogonUI\LastLoggedOnDisplayName` (e.g. `"Yamada Taro"`). `None`
    /// when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_logon_display_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn heartbeat_round_trips_through_json() {
        let hb = Heartbeat {
            pc_id: "pc-01".into(),
            at: chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap(),
            agent_version: "0.12.0".into(),
            hostname: Some("PC-01".into()),
            os_family: Some("windows".into()),
            agent_cpu_pct: Some(0.3),
            agent_rss_bytes: Some(45_000_000),
            agent_disk_read_bytes: Some(1024 * 1024),
            agent_disk_written_bytes: Some(512 * 1024),
            quarantined_versions: vec!["0.43.51".into()],
            last_logon_user: Some("EXAMPLE\\taro".into()),
            last_logon_display_name: Some("Yamada Taro".into()),
        };
        let json = serde_json::to_string(&hb).unwrap();
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pc_id, hb.pc_id);
        assert_eq!(back.at, hb.at);
        assert_eq!(back.agent_version, hb.agent_version);
        assert_eq!(back.hostname, hb.hostname);
        assert_eq!(back.os_family, hb.os_family);
        assert_eq!(back.agent_cpu_pct, hb.agent_cpu_pct);
        assert_eq!(back.agent_rss_bytes, hb.agent_rss_bytes);
        assert_eq!(back.agent_disk_read_bytes, hb.agent_disk_read_bytes);
        assert_eq!(back.agent_disk_written_bytes, hb.agent_disk_written_bytes);
        assert_eq!(back.quarantined_versions, hb.quarantined_versions);
        assert_eq!(back.last_logon_user, hb.last_logon_user);
        assert_eq!(back.last_logon_display_name, hb.last_logon_display_name);
    }

    #[test]
    fn heartbeat_empty_quarantine_is_omitted_on_the_wire() {
        let hb = Heartbeat {
            pc_id: "x".into(),
            at: chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap(),
            agent_version: "0.43.50".into(),
            hostname: None,
            os_family: None,
            agent_cpu_pct: None,
            agent_rss_bytes: None,
            agent_disk_read_bytes: None,
            agent_disk_written_bytes: None,
            quarantined_versions: Vec::new(),
            last_logon_user: None,
            last_logon_display_name: None,
        };
        let json = serde_json::to_string(&hb).unwrap();
        assert!(
            !json.contains("quarantined_versions"),
            "empty quarantine must be skipped on the wire: {json}",
        );
        // And a payload without the field still decodes to empty.
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert!(back.quarantined_versions.is_empty());
    }

    #[test]
    fn heartbeat_without_enrichment_still_decodes() {
        // Older agents sending only the v0.11 shape must still parse.
        let json = r#"{"pc_id":"x","at":"2026-05-16T00:00:00Z","agent_version":"0.11.5"}"#;
        let hb: Heartbeat = serde_json::from_str(json).unwrap();
        assert_eq!(hb.pc_id, "x");
        assert_eq!(hb.hostname, None);
        assert_eq!(hb.os_family, None);
        // v0.37 Part 2: perf fields are also optional and default
        // to None, so a pre-0.37 agent's heartbeat keeps decoding.
        assert_eq!(hb.agent_cpu_pct, None);
        assert_eq!(hb.agent_rss_bytes, None);
        assert_eq!(hb.agent_disk_read_bytes, None);
        assert_eq!(hb.agent_disk_written_bytes, None);
        // last-logon fields are optional too: a heartbeat that omits
        // them (older agent, non-Windows host) decodes to None.
        assert_eq!(hb.last_logon_user, None);
        assert_eq!(hb.last_logon_display_name, None);
    }
}
