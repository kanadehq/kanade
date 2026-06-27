//! Native Windows Event Log → `ObsEvent` reader (#841, PR2a).
//!
//! Replaces the `collect-winlog-events` PowerShell `Get-WinEvent` job with an
//! in-process `EvtQuery` poll, dropping the per-tick PowerShell process spawn
//! (the dominant cost of the old approach). The fleet's operational swimlane
//! lanes that ARE recorded in the Event Log — power (boot / shutdown / sleep /
//! resume), session (logon / logoff / lock / unlock), service start/stop — are
//! read straight from the log here; the one signal that ISN'T logged (active
//! vs idle) stays in [`crate::idle_sampler`].
//!
//! Per source row `(channel, provider?, event_id, kind)` (the same matrix the
//! PowerShell job hard-coded), each poll:
//!   1. `EvtQuery`s the channel for that Event ID since a 24h floor (a
//!      provider filter pins the IDs that collide across providers — Winlogon
//!      7001 vs SCM, Kernel-General 12/13, Kernel-Power 41/42/107/506/507);
//!   2. renders each record to XML (`EvtRender`) and parses out the
//!      EventRecordID / TimeCreated / EventData;
//!   3. skips records at/below the persisted per-source RecordID watermark,
//!      shapes the per-kind payload, and enqueues an `ObsEvent` to the shared
//!      `obs_outbox` (the drain publishes it);
//!   4. advances the watermark over each handled (enqueued or intentionally
//!      skipped) record — stopping at the first enqueue failure so that record
//!      is retried, not lost — and persists it.
//!
//! Gap-free for downtime < 24h (the watermark + RecordID skip de-dups within
//! the rolling window; the backend's `UNIQUE(pc_id, source, event_record_id)`
//! absorbs any re-send). An agent offline > 24h misses events older than 24h
//! before reconnect — the same trade the PowerShell job made (the timeline is
//! "what's happening now", not a forensic archive). #841 PR2b adds logon SID →
//! name translation; a future `collector` resource will make the source matrix
//! and cadence operator-tunable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use kanade_shared::wire::ObsEvent;
use serde_json::{Value, json};
use tracing::warn;

/// Poll cadence. The old PowerShell schedule fired every 10 min; the native
/// read is cheap enough (no process spawn) to halve that for a fresher
/// timeline without fleet impact.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How far back the query floor reaches. Bounds the per-poll scan AND seeds a
/// fresh agent with a sensible window instead of "every event since the box
/// was built". Hours, applied as `now - BOOTSTRAP_HOURS`.
const BOOTSTRAP_HOURS: i64 = 24;

/// One Event Log source row → one `obs_events` kind.
struct Source {
    /// Event Log channel (`System` / `Security`). Becomes `winlog:<channel>`
    /// as the `ObsEvent.source`.
    channel: &'static str,
    /// Provider name to pin, for the Event IDs that several providers reuse
    /// (an unpinned `EventID=12` query would scoop up every provider's ID 12).
    provider: Option<&'static str>,
    /// Event ID within the channel.
    id: u32,
    /// The `obs_events.kind` this row produces.
    kind: &'static str,
}

/// The source matrix — the `$sources` table the retired
/// `collect-winlog-events` PowerShell job hard-coded, carried over verbatim so
/// the native reader produces the same timeline. See its history
/// (#246 / #378 / #385, in git) for the per-row rationale: why each ID is
/// provider-pinned, why 42/506 both map to `sleep`, why 27 is hibernate-only.
const SOURCES: &[Source] = &[
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Winlogon"),
        id: 7001,
        kind: "logon",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Winlogon"),
        id: 7002,
        kind: "logoff",
    },
    Source {
        channel: "Security",
        provider: None,
        id: 4800,
        kind: "lock",
    },
    Source {
        channel: "Security",
        provider: None,
        id: 4801,
        kind: "unlock",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-General"),
        id: 12,
        kind: "boot",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-General"),
        id: 13,
        kind: "shutdown",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-Power"),
        id: 41,
        kind: "unexpected_shutdown",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-Power"),
        id: 42,
        kind: "sleep",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-Power"),
        id: 107,
        kind: "resume",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-Power"),
        id: 506,
        kind: "sleep",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-Power"),
        id: 507,
        kind: "resume",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Kernel-Boot"),
        id: 27,
        kind: "resume",
    },
    Source {
        channel: "System",
        provider: Some("Microsoft-Windows-Power-Troubleshooter"),
        id: 1,
        kind: "wake_detail",
    },
    Source {
        channel: "System",
        provider: None,
        id: 6005,
        kind: "log_service_started",
    },
    Source {
        channel: "System",
        provider: None,
        id: 6006,
        kind: "log_service_stopped",
    },
];

/// Per-source RecordID watermark, keyed by `<channel>:<id>:<kind>`. The Event
/// ID is in the key (not just channel:kind) because two IDs can share a kind
/// (42 and 506 are both `sleep`) and would otherwise share — and clobber — a
/// watermark.
type Watermarks = HashMap<String, u64>;

fn source_key(s: &Source) -> String {
    // Provider is part of the key so two rows sharing (channel, id, kind) but
    // pinned to different providers can't collide on one watermark. No two
    // rows do today (source_keys_are_unique enforces it), but the key
    // shouldn't silently depend on that staying true.
    format!(
        "{}:{}:{}:{}",
        s.channel,
        s.provider.unwrap_or(""),
        s.id,
        s.kind
    )
}

/// Build the `EvtQuery` XPath selecting this source's events at/after `since`.
/// All inputs are trusted — the channel/provider/id are compile-time constants
/// and `since` is a formatted RFC3339 stamp — so the interpolation carries no
/// injection risk (there is no user input anywhere in the query).
fn build_query(s: &Source, since: DateTime<Utc>) -> String {
    let since = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let provider = match s.provider {
        Some(p) => format!("Provider[@Name='{p}'] and "),
        None => String::new(),
    };
    format!(
        "*[System[{provider}(EventID={id}) and TimeCreated[@SystemTime>='{since}']]]",
        id = s.id
    )
}

/// The fields the shaper needs out of one rendered event record.
struct ParsedEvent {
    record_id: u64,
    at: DateTime<Utc>,
    /// `<Data Name="X">v</Data>` fields (Security events, wake_detail).
    named: HashMap<String, String>,
    /// `<Data>v</Data>` values in document order (Winlogon, Kernel-Boot).
    positional: Vec<String>,
}

/// First child element with the given LOCAL name, namespace-agnostic — event
/// XML carries a default namespace, so matching the qualified name would miss
/// (`has_tag_name("System")` checks the no-namespace name and never matches).
fn child<'a>(n: roxmltree::Node<'a, 'a>, local: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == local)
}

/// Parse one rendered event XML into [`ParsedEvent`]. `None` when the
/// mandatory System fields (EventRecordID, TimeCreated) are missing or
/// unparseable — such a record can't be de-duped or placed on the timeline,
/// so it's dropped (and re-examined next poll, since its RecordID is unknown
/// and the watermark can't advance past it).
fn parse_event_xml(xml: &str) -> Option<ParsedEvent> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let system = child(doc.root_element(), "System")?;
    let record_id = child(system, "EventRecordID")?
        .text()?
        .trim()
        .parse::<u64>()
        .ok()?;
    let at = DateTime::parse_from_rfc3339(child(system, "TimeCreated")?.attribute("SystemTime")?)
        .ok()?
        .with_timezone(&Utc);
    let mut named = HashMap::new();
    let mut positional = Vec::new();
    if let Some(event_data) = child(doc.root_element(), "EventData") {
        for d in event_data
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "Data")
        {
            let v = d.text().unwrap_or("").to_string();
            match d.attribute("Name") {
                Some(name) => {
                    named.insert(name.to_string(), v);
                }
                None => positional.push(v),
            }
        }
    }
    Some(ParsedEvent {
        record_id,
        at,
        named,
        positional,
    })
}

/// Outcome of shaping a record: either an `ObsEvent` payload to emit, or a
/// skip for a record that matched the query but isn't one we surface (a
/// Kernel-Boot 27 that isn't a hibernate resume). A skip still advances the
/// watermark so the record isn't re-examined.
enum Shaped {
    Emit(Value),
    Skip,
}

/// Shape the per-kind payload, mirroring the PowerShell collector's matrix.
/// PR2a emits the logon/logoff user as the RAW SID; #841 PR2b adds the
/// SID → leaf-name translation (the part that needs careful LSA/domain
/// gating).
fn shape(s: &Source, ev: &ParsedEvent) -> Shaped {
    match s.id {
        // Winlogon 7001/7002: positional Data — [0] = TSId (session),
        // [1] = UserSid. PR2a keeps `user` as the raw SID.
        7001 | 7002 => {
            let sid = ev.positional.get(1).map(String::as_str);
            let session_id = ev
                .positional
                .first()
                .and_then(|v| v.trim().parse::<i64>().ok());
            Shaped::Emit(json!({ "user": sid, "sid": sid, "session_id": session_id }))
        }
        // Security 4800/4801: named Data — TargetUserName is already a name.
        4800 | 4801 => {
            let user = ev.named.get("TargetUserName").map(String::as_str);
            let session_id = ev
                .named
                .get("SessionId")
                .and_then(|v| v.trim().parse::<i64>().ok());
            Shaped::Emit(json!({ "user": user, "session_id": session_id }))
        }
        // Modern Standby enter/exit — same kinds as 42/107, flagged.
        506 | 507 => Shaped::Emit(json!({ "standby": "modern" })),
        // Kernel-Boot 27: positional [0] = BootType. Only 0x2 (resume from
        // hibernation) surfaces, as `resume`; 0x0 cold / 0x1 fast-startup are
        // already covered by `boot` and would double every power-on.
        27 => match ev
            .positional
            .first()
            .and_then(|v| v.trim().parse::<i64>().ok())
        {
            Some(2) => Shaped::Emit(json!({ "from": "hibernate" })),
            _ => Shaped::Skip,
        },
        // Power-Troubleshooter 1 (wake_detail): named Data.
        1 => {
            let wake_source = ev
                .named
                .get("WakeSourceText")
                .filter(|v| !v.trim().is_empty())
                .or_else(|| ev.named.get("WakeSourceType"))
                .map(String::as_str);
            Shaped::Emit(json!({
                "sleep_start": ev.named.get("SleepTime").map(String::as_str),
                "wake_time": ev.named.get("WakeTime").map(String::as_str),
                "wake_source": wake_source,
            }))
        }
        // boot / shutdown / unexpected_shutdown / 42 / 107 / 6005 / 6006 —
        // the bare presence is the whole signal.
        _ => Shaped::Emit(Value::Null),
    }
}

fn load_watermarks(path: &Path) -> Watermarks {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(error = %e, "winlog: watermark file corrupt — bootstrapping fresh");
            Watermarks::new()
        }),
        // Missing file → first run (silent, expected). Any OTHER read error
        // (permissions, disk) bootstraps fresh too, but is logged — otherwise
        // a persistently-unreadable file would silently re-bootstrap the 24h
        // window on every restart with no signal.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            warn!(error = %e, "winlog: watermark file unreadable — bootstrapping fresh");
            Watermarks::new()
        }
        Err(_) => Watermarks::new(),
    }
}

/// Persist the watermark map atomically (tmp + rename), mirroring the outbox's
/// crash-safe write. Best-effort: a write failure is logged and the in-memory
/// map keeps advancing, so collection continues (worst case a re-send the
/// backend de-dups).
fn save_watermarks(path: &Path, w: &Watermarks) {
    let tmp = path.with_extension("json.tmp");
    let res = serde_json::to_vec(w)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| std::fs::write(&tmp, bytes).map_err(anyhow::Error::from))
        .and_then(|()| std::fs::rename(&tmp, path).map_err(anyhow::Error::from));
    if let Err(e) = res {
        warn!(error = %e, "winlog: watermark persist failed");
    }
}

/// One poll over every source. Takes ownership of the watermark map and
/// returns it updated, so the caller can run this under `spawn_blocking`
/// (the `EvtQuery` FFI + render is synchronous and shouldn't block the async
/// runtime).
fn poll_once(pc_id: &str, dir: &Path, mut watermarks: Watermarks) -> Watermarks {
    let since = Utc::now() - chrono::Duration::hours(BOOTSTRAP_HOURS);
    for s in SOURCES {
        let key = source_key(s);
        let cutoff = watermarks.get(&key).copied();
        let xmls = match query_events(s.channel, &build_query(s, since)) {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, source = %key, "winlog: query failed");
                continue;
            }
        };
        // Ascending RecordID so the projector sees source order and the
        // watermark advances monotonically.
        let mut events: Vec<ParsedEvent> = xmls.iter().filter_map(|x| parse_event_xml(x)).collect();
        events.sort_by_key(|e| e.record_id);

        // The watermark advances only over records we've actually dealt with
        // (enqueued, or intentionally skipped). An enqueue failure stops the
        // walk WITHOUT advancing past the failed record, so it — and every
        // later record this poll — is retried next time rather than silently
        // lost. Records arrive ascending, so the watermark is the high-water of
        // a contiguous handled prefix.
        let mut max_seen = cutoff;
        for ev in &events {
            if cutoff.is_some_and(|c| ev.record_id <= c) {
                continue; // already emitted on an earlier poll
            }
            let payload = match shape(s, ev) {
                Shaped::Emit(p) => p,
                Shaped::Skip => {
                    // Deliberately ignored (e.g. a non-hibernate Kernel-Boot
                    // 27) — safe to advance past.
                    max_seen = Some(max_seen.map_or(ev.record_id, |m| m.max(ev.record_id)));
                    continue;
                }
            };
            let event = ObsEvent {
                pc_id: pc_id.to_string(),
                at: ev.at,
                kind: s.kind.to_string(),
                source: format!("winlog:{}", s.channel),
                event_record_id: Some(ev.record_id.to_string()),
                payload,
            };
            if let Err(e) = crate::obs_outbox::enqueue(dir, &event) {
                warn!(error = %e, kind = s.kind, "winlog: enqueue failed — retrying next poll");
                break; // don't advance the watermark past an event we failed to queue
            }
            max_seen = Some(max_seen.map_or(ev.record_id, |m| m.max(ev.record_id)));
        }
        if let Some(m) = max_seen {
            if Some(m) != cutoff {
                watermarks.insert(key, m);
            }
        }
    }
    watermarks
}

/// Long-lived reader: poll every [`POLL_INTERVAL`], persisting the watermark
/// after each pass. Spawned once at agent start; runs for the process lifetime.
pub async fn run(pc_id: String, obs_outbox_dir: PathBuf) {
    if let Err(e) = crate::obs_outbox::ensure_outbox_dir(&obs_outbox_dir) {
        warn!(error = %e, "winlog: outbox dir — events may be dropped until it exists");
    }
    let watermark_path =
        kanade_shared::default_paths::data_dir().join("winlog-reader-watermark.json");
    let mut watermarks = load_watermarks(&watermark_path);
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    // Skip, not Burst: a suspend/resume shouldn't fire catch-up polls
    // back-to-back — one poll on resume, then the normal cadence.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let pc = pc_id.clone();
        let dir = obs_outbox_dir.clone();
        let wpath = watermark_path.clone();
        let wm = std::mem::take(&mut watermarks);
        watermarks = match tokio::task::spawn_blocking(move || {
            let wm = poll_once(&pc, &dir, wm);
            save_watermarks(&wpath, &wm);
            wm
        })
        .await
        {
            Ok(wm) => wm,
            Err(e) => {
                // A panic in the blocking poll shouldn't kill the reader;
                // reload the watermark from disk and carry on next tick.
                warn!(error = %e, "winlog: poll task failed");
                load_watermarks(&watermark_path)
            }
        };
    }
}

/// Render every record matching `query` on `channel` to its event XML.
/// Windows only; other targets (CI / dev — no production agents) return an
/// empty set so the reader is a no-op there.
#[cfg(target_os = "windows")]
fn query_events(channel: &str, query: &str) -> anyhow::Result<Vec<String>> {
    use windows::Win32::System::EventLog::{
        EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryForwardDirection,
    };
    use windows::core::HSTRING;

    // SAFETY: `EvtQuery` returns an owned result-set handle (closed below on
    // every path). The channel/query HSTRINGs outlive the call.
    let result_set = unsafe {
        EvtQuery(
            None,
            &HSTRING::from(channel),
            &HSTRING::from(query),
            EvtQueryChannelPath.0 | EvtQueryForwardDirection.0,
        )
    }?;

    let mut out = Vec::new();
    const BATCH: usize = 32;
    loop {
        // EvtNext takes `&mut [isize]` (windows-rs hands back EVT_HANDLE as a
        // raw isize here); wrap each into EVT_HANDLE for render/close.
        let mut handles = [0isize; BATCH];
        let mut returned: u32 = 0;
        // SAFETY: `handles` is a valid, fully-initialised array; `EvtNext`
        // fills the first `returned` slots with owned event handles. A
        // non-Ok return (incl. ERROR_NO_MORE_ITEMS) ends the walk.
        // Timeout 0: it's ignored for a query result set (only meaningful for
        // a real-time subscription) — the results are already materialised, so
        // EvtNext returns immediately.
        let ok = unsafe { EvtNext(result_set, &mut handles, 0, 0, &mut returned) }.is_ok();
        if !ok {
            break;
        }
        for &h in handles.iter().take(returned as usize) {
            let event = windows::Win32::System::EventLog::EVT_HANDLE(h);
            if let Some(xml) = render_event_xml(event) {
                out.push(xml);
            }
            // SAFETY: each event handle EvtNext handed back is owned by us.
            unsafe {
                let _ = EvtClose(event);
            }
        }
    }
    // SAFETY: result-set handle owned by us; closed exactly once.
    unsafe {
        let _ = EvtClose(result_set);
    }
    Ok(out)
}

/// Render one event handle to its XML string via the two-call `EvtRender`
/// buffer-sizing pattern. `None` on any render failure (the record is dropped,
/// not fatal).
#[cfg(target_os = "windows")]
fn render_event_xml(event: windows::Win32::System::EventLog::EVT_HANDLE) -> Option<String> {
    use windows::Win32::System::EventLog::{EvtRender, EvtRenderEventXml};

    let mut used: u32 = 0;
    let mut props: u32 = 0;
    // First call sizes the buffer: it fails with ERROR_INSUFFICIENT_BUFFER and
    // writes the required BYTE count into `used`.
    // SAFETY: null buffer with size 0 is the documented size-probe form.
    unsafe {
        let _ = EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            0,
            None,
            &mut used,
            &mut props,
        );
    }
    if used == 0 {
        return None;
    }
    // `used` is bytes; the buffer is UTF-16. Round up to whole u16s.
    let mut buf = vec![0u16; (used as usize).div_ceil(2)];
    // SAFETY: `buf` is `used` bytes; `EvtRender` writes a NUL-terminated
    // UTF-16 string into it. We read only up to the NUL below.
    unsafe {
        EvtRender(
            None,
            event,
            EvtRenderEventXml.0,
            used,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut used,
            &mut props,
        )
    }
    .ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    // `from_utf16_lossy` maps any unpaired surrogate to U+FFFD. EvtRender
    // output should always be valid UTF-16; if it somehow isn't (corrupt
    // .evtx / partial render), a replacement char may make the XML malformed,
    // which `parse_event_xml` then drops as a None — acceptable for one bad
    // record, and not worth failing the whole poll over.
    Some(String::from_utf16_lossy(&buf[..end]))
}

#[cfg(not(target_os = "windows"))]
fn query_events(_channel: &str, _query: &str) -> anyhow::Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal event XML with the default namespace event records carry, so
    // the namespace-agnostic `child()` lookup is exercised for real.
    fn ev_xml(system_extra: &str, event_data: &str) -> String {
        format!(
            "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>\
               <System>{system_extra}\
                 <TimeCreated SystemTime='2026-06-27T01:02:03.123456700Z'/>\
                 <EventRecordID>4242</EventRecordID>\
               </System>{event_data}</Event>"
        )
    }

    #[test]
    fn build_query_pins_provider_and_time() {
        let s = &SOURCES[0]; // Winlogon 7001 logon
        let q = build_query(
            s,
            DateTime::parse_from_rfc3339("2026-06-26T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert!(
            q.contains("Provider[@Name='Microsoft-Windows-Winlogon']"),
            "q: {q}"
        );
        assert!(q.contains("(EventID=7001)"), "q: {q}");
        assert!(
            q.contains("TimeCreated[@SystemTime>='2026-06-26T00:00:00.000Z']"),
            "q: {q}"
        );
        // A provider-less row omits the Provider clause.
        let bare = build_query(&SOURCES[13], Utc::now()); // 6005 log_service_started
        assert!(!bare.contains("Provider"), "q: {bare}");
        assert!(bare.contains("(EventID=6005)"), "q: {bare}");
    }

    #[test]
    fn parse_extracts_system_and_event_data() {
        let xml = ev_xml(
            "",
            "<EventData><Data Name='TargetUserName'>alice</Data>\
                        <Data Name='SessionId'>3</Data></EventData>",
        );
        let p = parse_event_xml(&xml).expect("parse");
        assert_eq!(p.record_id, 4242);
        assert_eq!(p.at.to_rfc3339(), "2026-06-27T01:02:03.123456700+00:00");
        assert_eq!(
            p.named.get("TargetUserName").map(String::as_str),
            Some("alice")
        );
        assert_eq!(p.named.get("SessionId").map(String::as_str), Some("3"));
        assert!(p.positional.is_empty());
    }

    #[test]
    fn parse_rejects_missing_system_fields() {
        // No EventRecordID → unusable (can't dedup / place it).
        let xml = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>\
                     <System><TimeCreated SystemTime='2026-06-27T01:02:03Z'/></System></Event>";
        assert!(parse_event_xml(xml).is_none());
        assert!(parse_event_xml("not xml at all").is_none());
    }

    fn parse(system_extra: &str, event_data: &str) -> ParsedEvent {
        parse_event_xml(&ev_xml(system_extra, event_data)).expect("parse")
    }

    #[test]
    fn shape_logon_uses_positional_sid_and_session() {
        let s = &SOURCES[0]; // 7001 logon
        let ev = parse(
            "",
            "<EventData><Data>2</Data><Data>S-1-5-21-1-2-3-1001</Data></EventData>",
        );
        let Shaped::Emit(p) = shape(s, &ev) else {
            panic!("expected emit")
        };
        assert_eq!(p["session_id"], json!(2));
        assert_eq!(p["sid"], json!("S-1-5-21-1-2-3-1001"));
        // PR2a: user is the raw SID until PR2b's translation.
        assert_eq!(p["user"], json!("S-1-5-21-1-2-3-1001"));
    }

    #[test]
    fn shape_lock_uses_named_fields() {
        let s = &SOURCES[2]; // 4800 lock
        let ev = parse(
            "",
            "<EventData><Data Name='TargetUserName'>bob</Data><Data Name='SessionId'>1</Data></EventData>",
        );
        let Shaped::Emit(p) = shape(s, &ev) else {
            panic!("expected emit")
        };
        assert_eq!(p["user"], json!("bob"));
        assert_eq!(p["session_id"], json!(1));
    }

    #[test]
    fn shape_modern_standby_flags_payload() {
        let s = &SOURCES[9]; // 506 sleep (modern standby)
        let ev = parse("", "");
        let Shaped::Emit(p) = shape(s, &ev) else {
            panic!("expected emit")
        };
        assert_eq!(p, json!({ "standby": "modern" }));
    }

    #[test]
    fn shape_kernel_boot_27_only_emits_hibernate_resume() {
        let s = &SOURCES[11]; // 27 resume (hibernate gate)
        // BootType 0x2 → resume from hibernate.
        let hib = parse("", "<EventData><Data>2</Data></EventData>");
        let Shaped::Emit(p) = shape(s, &hib) else {
            panic!("expected emit")
        };
        assert_eq!(p, json!({ "from": "hibernate" }));
        // BootType 0x0 (cold) / 0x1 (fast startup) → skip (covered by `boot`).
        assert!(matches!(
            shape(s, &parse("", "<EventData><Data>0</Data></EventData>")),
            Shaped::Skip
        ));
        assert!(matches!(
            shape(s, &parse("", "<EventData><Data>1</Data></EventData>")),
            Shaped::Skip
        ));
    }

    #[test]
    fn shape_wake_detail_prefers_source_text_with_fallback() {
        let s = &SOURCES[12]; // 1 wake_detail
        let ev = parse(
            "",
            "<EventData><Data Name='SleepTime'>2026-06-27T00:00:00Z</Data>\
                        <Data Name='WakeTime'>2026-06-27T01:00:00Z</Data>\
                        <Data Name='WakeSourceType'>5</Data></EventData>",
        );
        let Shaped::Emit(p) = shape(s, &ev) else {
            panic!("expected emit")
        };
        assert_eq!(p["sleep_start"], json!("2026-06-27T00:00:00Z"));
        assert_eq!(p["wake_time"], json!("2026-06-27T01:00:00Z"));
        // No WakeSourceText → falls back to WakeSourceType.
        assert_eq!(p["wake_source"], json!("5"));
    }

    #[test]
    fn shape_bare_presence_is_null_payload() {
        let s = &SOURCES[4]; // 12 boot
        let Shaped::Emit(p) = shape(s, &parse("", "")) else {
            panic!("expected emit")
        };
        assert_eq!(p, Value::Null);
    }

    #[test]
    fn watermarks_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wm.json");
        // Missing file → empty (first run).
        assert!(load_watermarks(&path).is_empty());
        let mut w = Watermarks::new();
        w.insert("System:7001:logon".into(), 99);
        save_watermarks(&path, &w);
        assert_eq!(load_watermarks(&path).get("System:7001:logon"), Some(&99));
        // Corrupt file → fresh (not a hard error).
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load_watermarks(&path).is_empty());
    }

    #[test]
    fn source_keys_are_unique() {
        // Two IDs share a kind (42/506 sleep, 107/507/27 resume) — the key
        // includes the ID so their watermarks don't collide.
        let mut keys: Vec<String> = SOURCES.iter().map(source_key).collect();
        keys.sort();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate source key");
    }
}
