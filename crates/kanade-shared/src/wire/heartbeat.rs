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
    /// #1165: the command-signing keys this agent currently trusts, each as
    /// `kid:fingerprint`.
    ///
    /// Reported so "which machines still trust the old key" is answerable.
    /// Without it, retiring a key is a guess: an agent that never received the
    /// replacement rejects every command at stage 3, and there is no way to
    /// know it was going to before it does.
    ///
    /// The fingerprint half (#1229) answers a question the id cannot: **do two
    /// machines holding the same id hold the same key**. The id is chosen by
    /// whoever wrote the ring, so a mistyped paste, a same-`kid` re-mint, or a
    /// hand-edited registry value produces a host that reports the expected id,
    /// refuses every command once enforcement is on, and never self-heals —
    /// the reload-on-unknown-key path (#1186) does not fire, because the key
    /// *is* present, just wrong.
    ///
    /// One flat string rather than a nested object on purpose. The projected
    /// column is a JSON array queried with `LIKE`, because the read-only query
    /// path rejects table-valued functions (`json_each`) — so
    /// `LIKE '%"backend-2026…:3f2a…"%'` pins the exact key with the machinery
    /// that already exists, while `LIKE '%"backend-2026…:%'` still asks the
    /// id-only question. Nothing parses these back apart; they are matched.
    ///
    /// `Option<Vec<_>>` rather than a plain `Vec` with
    /// `skip_serializing_if = "Vec::is_empty"` — the shape
    /// [`Self::quarantined_versions`] uses — because **empty is the state this
    /// exists to surface**. Skipping an empty list would put "this agent holds
    /// no keys" and "this agent is too old to say" on the wire as the same
    /// thing, and they need opposite responses: provision the first one, and
    /// upgrade the second before you can even ask. So:
    ///
    /// * `None` — the agent predates this field. Unknown, not empty.
    /// * `Some([])` — reporting, and holds nothing. This is the work queue.
    /// * `Some([kid, ..])` — what it will actually accept right now.
    ///
    /// It reports the **in-memory** ring, not the registry. Those differ
    /// between a key landing on disk and the reload that picks it up (#1186),
    /// and the useful answer is what this agent would accept if a command
    /// arrived now — reporting the file would describe a machine that does not
    /// exist yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_keys: Option<Vec<String>>,
    /// #1250: whether this agent is refusing commands it cannot verify.
    ///
    /// [`Self::command_keys`] made key *distribution* enumerable; this makes
    /// *enforcement* enumerable, which nothing else does. It cannot be inferred
    /// from the signature outcomes: in normal operation every command is
    /// signed, so an enforcing host and a non-enforcing one both report
    /// `command_signature_ok`. The two are observationally identical until
    /// something unsigned arrives — which is exactly the event nobody wants to
    /// stage across a fleet to find out. Nor from `command_keys`: a host can
    /// hold a perfect ring and not be enforcing, which is what every machine is
    /// doing today.
    ///
    /// Three states, for the same reason as `command_keys`:
    ///
    /// * `None` — the agent predates this field. Unknown, not "no".
    /// * `Some(false)` — reporting, and not enforcing. **This is the queue.**
    /// * `Some(true)` — refusing unverified commands right now.
    ///
    /// The **effective** state, not the configured one: an agent declines to
    /// enforce on an empty ring (refusing everything would include the command
    /// that restores the keys), so a host in that state reports `false` however
    /// its registry reads. Reporting the configured value would describe a
    /// machine that does not exist — the same rule that makes `command_keys`
    /// report memory rather than disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcing: Option<bool>,
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
            command_keys: Some(vec!["backend-20260728".into()]),
            enforcing: Some(false),
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
        assert_eq!(back.command_keys, hb.command_keys);
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
            command_keys: None,
            enforcing: None,
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
    fn an_empty_keyring_is_reported_rather_than_omitted() {
        // The distinction the whole field exists for. `quarantined_versions`
        // skips an empty list because "nothing quarantined" is the boring
        // default; an empty *keyring* is the opposite — it is the machine an
        // operator has to act on. If it were skipped, it would arrive looking
        // exactly like an agent too old to report at all, and the two need
        // opposite responses (provision it / upgrade it first).
        let hb = Heartbeat {
            pc_id: "x".into(),
            at: chrono::Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0).unwrap(),
            agent_version: "0.44.36".into(),
            hostname: None,
            os_family: None,
            agent_cpu_pct: None,
            agent_rss_bytes: None,
            agent_disk_read_bytes: None,
            agent_disk_written_bytes: None,
            quarantined_versions: Vec::new(),
            last_logon_user: None,
            last_logon_display_name: None,
            command_keys: Some(Vec::new()),
            enforcing: Some(false),
        };
        let json = serde_json::to_string(&hb).unwrap();
        assert!(
            json.contains("command_keys"),
            "an empty keyring must still reach the backend: {json}"
        );
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.command_keys, Some(Vec::new()));

        // And an agent that predates the field stays distinguishable from it.
        let old = r#"{"pc_id":"x","at":"2026-05-16T00:00:00Z","agent_version":"0.44.35"}"#;
        let hb: Heartbeat = serde_json::from_str(old).unwrap();
        assert_eq!(hb.command_keys, None, "absent must not decode as empty");
    }

    #[test]
    fn not_enforcing_is_reported_rather_than_omitted() {
        // #1250, and the same trap as the keyring above: `false` is the state
        // worth acting on, so it must not be skipped. A `skip_serializing_if`
        // that dropped it would put "this host is not enforcing" and "this host
        // is too old to say" on the wire as the same absence — and stage 3's
        // remaining work is exactly the first set.
        let hb = Heartbeat {
            pc_id: "x".into(),
            at: chrono::Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
            agent_version: "0.45.1".into(),
            hostname: None,
            os_family: None,
            agent_cpu_pct: None,
            agent_rss_bytes: None,
            agent_disk_read_bytes: None,
            agent_disk_written_bytes: None,
            quarantined_versions: Vec::new(),
            last_logon_user: None,
            last_logon_display_name: None,
            command_keys: Some(vec!["backend-20260728:75b4c8f44e18012d".into()]),
            enforcing: Some(false),
        };
        let json = serde_json::to_string(&hb).unwrap();
        assert!(
            json.contains("\"enforcing\":false"),
            "a host that is not enforcing must say so: {json}"
        );
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.enforcing, Some(false));

        // The pairing this field exists to expose: a complete ring proves
        // nothing about enforcement, so neither value can be read off the
        // other. This heartbeat is every machine in the fleet today.
        assert!(back.command_keys.is_some_and(|k| !k.is_empty()));

        let old = r#"{"pc_id":"x","at":"2026-08-01T00:00:00Z","agent_version":"0.45.1"}"#;
        let hb: Heartbeat = serde_json::from_str(old).unwrap();
        assert_eq!(hb.enforcing, None, "absent must not decode as false");
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
