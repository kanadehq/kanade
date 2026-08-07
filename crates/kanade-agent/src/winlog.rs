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
//! "what's happening now", not a forensic archive).
//!
//! Winlogon logon/logoff carry the user as a SID; [`SidResolver`] translates
//! it to a leaf account name, gated on domain reachability so an off-network
//! box doesn't stall on an unreachable DC (see [`should_translate`]). A future
//! `collector` resource (#841) will make the source matrix and cadence
//! operator-tunable.

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
    /// `<Data Name="X">v</Data>` fields. This is what MANIFEST-BASED
    /// providers emit, which is every provider this reader queries — the
    /// names are in the provider manifest and appear in the rendered XML.
    named: HashMap<String, String>,
    /// `<Data>v</Data>` values in document order, for classic (mc-style)
    /// providers that ship no manifest.
    ///
    /// Do not reach for this because a field "looks positional" in a
    /// PowerShell collector: `$e.Properties[i]` is a positional array
    /// REGARDLESS of whether the manifest names the field, so an index that
    /// worked there says nothing about the XML. Porting one over is how
    /// Winlogon 7001/7002 and Kernel-Boot 27 came to read an always-empty
    /// vector, emitting all-null logon payloads and never a hibernate resume
    /// (#1210).
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

/// One EventData field: by NAME, falling back to a positional index.
///
/// Named is what every provider this reader queries actually emits — they
/// are manifest-based, so the manifest's field names appear in the rendered
/// XML. Reading position instead is #1210: an always-empty vector, all-null
/// logon payloads, and a hibernate resume that never fired.
///
/// The positional arm is for a classic (mc-style) provider that ships no
/// manifest, in the documented field order. It is not what Windows sends for
/// Winlogon or Kernel-Boot, and no fixture pretends otherwise — it exists so
/// an unexpected shape degrades to the pre-#1210 behaviour rather than to
/// nulls.
fn field<'a>(ev: &'a ParsedEvent, name: &str, idx: usize) -> Option<&'a str> {
    ev.named
        .get(name)
        .map(String::as_str)
        .or_else(|| ev.positional.get(idx).map(String::as_str))
}

/// Shape the per-kind payload, mirroring the PowerShell collector's matrix.
/// `resolve_user` turns a Winlogon SID into a display name (a leaf account
/// name when it can be looked up, else the raw SID); it's only invoked for
/// logon/logoff, the one kind whose user is a SID rather than a name.
fn shape(s: &Source, ev: &ParsedEvent, mut resolve_user: impl FnMut(&str) -> String) -> Shaped {
    match s.id {
        // Winlogon 7001/7002:
        //
        //   <Data Name='TSId'>1</Data>
        //   <Data Name='UserSid'>S-1-5-21-…</Data>
        //
        // `user` is the resolved name; `sid` keeps the raw SID for forensics
        // regardless of whether translation succeeded.
        7001 | 7002 => {
            let sid = field(ev, "UserSid", 1);
            let user = sid.map(&mut resolve_user);
            let session_id = field(ev, "TSId", 0).and_then(|v| v.trim().parse::<i64>().ok());
            Shaped::Emit(json!({ "user": user, "sid": sid, "session_id": session_id }))
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
        // Kernel-Boot 27: `BootType`, alongside `LoadOptions`. Only 0x2
        // (resume from hibernation) surfaces, as `resume`; 0x0 cold and 0x1
        // fast-startup are already covered by `boot` and would double every
        // power-on.
        //
        // This one failed silently in a way the logon bug did not: an absent
        // `BootType` parses to `None` and falls through to `Skip`, so the arm
        // simply never fired and no null payload showed up to notice.
        27 => match field(ev, "BootType", 0).and_then(|v| v.trim().parse::<i64>().ok()) {
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

/// Host context that decides whether a SID is safe to translate without
/// risking a slow LSA round-trip to an unreachable domain controller.
#[derive(Clone, Copy, Default)]
struct DomainCtx {
    /// The box is joined to an AD domain (vs a workgroup).
    domain_joined: bool,
    /// The domain's DNS name currently resolves (a cheap proxy for "a DC is
    /// reachable" — recomputed each poll, so a laptop that comes back on-net
    /// starts resolving domain users again).
    domain_reachable: bool,
}

/// `true` for a domain-style account SID (`S-1-5-21-…`) — as opposed to a
/// well-known / built-in SID (`S-1-5-18` LocalSystem, `S-1-5-19/20`, …) which
/// always resolves via a purely local lookup.
fn is_account_sid(sid: &str) -> bool {
    sid.starts_with("S-1-5-21-")
}

/// Whether to attempt an LSA translation for `sid`. A well-known SID, or any
/// account SID on a workgroup box (all such are local), resolves locally and
/// is always safe. A domain account SID needs the DC, so it's only attempted
/// when the domain currently resolves — otherwise the lookup could block for
/// tens of seconds on an off-network machine.
///
/// Trade-off vs the old PowerShell collector: it also translated *local*
/// accounts on a domain-joined box while offline (by comparing against the
/// machine's account-domain SID prefix). We skip that LSA-policy lookup; the
/// only regression is a local-account logon on a domain box *while the DC is
/// unreachable*, which keeps its raw SID — rare, and it resolves on the next
/// poll once the machine is back on-network.
fn should_translate(sid: &str, ctx: DomainCtx) -> bool {
    if !is_account_sid(sid) {
        return true; // well-known / built-in → local
    }
    if !ctx.domain_joined {
        return true; // workgroup → every account SID is local
    }
    ctx.domain_reachable // domain account → only when the DC is reachable
}

/// Resolves Winlogon SIDs to account names, caching successful lookups across
/// polls (a fleet logs the same handful of users in repeatedly). A failed or
/// gated lookup falls back to the raw SID and is NOT cached, so it's retried
/// on a later poll (e.g. once an off-network laptop reconnects).
#[derive(Default)]
struct SidResolver {
    cache: HashMap<String, String>,
}

impl SidResolver {
    fn resolve(&mut self, sid: &str, ctx: DomainCtx) -> String {
        if let Some(name) = self.cache.get(sid) {
            return name.clone();
        }
        if should_translate(sid, ctx) {
            if let Some(name) = translate_sid(sid) {
                self.cache.insert(sid.to_string(), name.clone());
                return name;
            }
        }
        sid.to_string()
    }
}

/// Probe the current host's domain context. Windows only; elsewhere reports
/// "not domain-joined" so the resolver treats every SID as locally resolvable
/// (and `translate_sid` is itself a no-op off Windows anyway).
#[cfg(target_os = "windows")]
fn domain_ctx() -> DomainCtx {
    match dns_domain() {
        Some(domain) if !domain.is_empty() => DomainCtx {
            domain_joined: true,
            domain_reachable: domain_reachable(&domain),
        },
        _ => DomainCtx::default(),
    }
}

#[cfg(not(target_os = "windows"))]
fn domain_ctx() -> DomainCtx {
    DomainCtx::default()
}

/// The box's AD DNS domain via `GetComputerNameExW(ComputerNameDnsDomain)`,
/// or `None` (empty) on a workgroup machine.
#[cfg(target_os = "windows")]
fn dns_domain() -> Option<String> {
    use windows::Win32::System::SystemInformation::{ComputerNameDnsDomain, GetComputerNameExW};
    use windows::core::PWSTR;

    let mut len: u32 = 0;
    // First call sizes the buffer (fails with ERROR_MORE_DATA, sets `len` to
    // the needed char count INCLUDING the NUL — so even a workgroup box's empty
    // domain reports len = 1, not 0).
    // SAFETY: null buffer + size 0 is the documented size-probe form.
    unsafe {
        let _ = GetComputerNameExW(ComputerNameDnsDomain, None, &mut len);
    }
    if len == 0 {
        // `len` left untouched → an unexpected failure, not the workgroup case
        // (which reports len = 1). Treat as no-domain either way.
        return Some(String::new());
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` chars; the call fills it with a NUL-terminated
    // string and writes the char count (excluding NUL) back into `len`.
    unsafe {
        GetComputerNameExW(
            ComputerNameDnsDomain,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// A cheap, bounded check that the AD domain currently resolves — a proxy for
/// "a DC is reachable". Runs the blocking `getaddrinfo` on a throwaway thread
/// with a 2s budget so an unreachable domain can't stall the poll (a lingering
/// thread finishes on its own once the OS resolver gives up).
///
/// Resolves the domain APEX, which in standard AD-integrated DNS carries an
/// A/AAAA record per DC (the same records `ping <domain>` / `\\<domain>\sysvol`
/// rely on) — a fast, dependency-free proxy. The proper AD-aware checks are
/// heavier: `DsGetDcNameW` itself blocks on discovery (the stall we're
/// avoiding), and SRV lookup (`_ldap._tcp.dc._msdcs.<domain>`) needs a real DNS
/// resolver. A false negative (healthy DC SRV but no apex host record — a
/// non-standard setup) just keeps the logon `user` as the raw SID, retried each
/// poll; switch to SRV discovery here if a real environment hits that.
// Only reached from the Windows `domain_ctx`; the platform-neutral body would
// otherwise be flagged dead off Windows.
#[cfg(target_os = "windows")]
fn domain_reachable(domain: &str) -> bool {
    use std::net::ToSocketAddrs;
    use std::sync::mpsc;

    // ToSocketAddrs needs a port; the value is irrelevant — we only care that
    // the name resolves.
    let target = format!("{domain}:0");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let resolved = target
            .as_str()
            .to_socket_addrs()
            .map(|mut addrs| addrs.next().is_some())
            .unwrap_or(false);
        let _ = tx.send(resolved);
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false)
}

/// Translate a string SID to its leaf account name (just the name, no
/// `DOMAIN\` prefix) via `ConvertStringSidToSidW` + `LookupAccountSidW`.
/// `None` on any failure — an unresolvable SID, or a domain SID whose DC the
/// caller didn't gate out and which then couldn't be reached. Windows only.
#[cfg(target_os = "windows")]
fn translate_sid(sid: &str) -> Option<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows::Win32::Security::{LookupAccountSidW, PSID, SidTypeUser};
    use windows::core::{HSTRING, PWSTR};

    let wide = HSTRING::from(sid);
    let mut psid = PSID::default();
    // SAFETY: ConvertStringSidToSidW LocalAllocs the PSID; we LocalFree it on
    // every path below. `wide` outlives the call.
    if unsafe { ConvertStringSidToSidW(&wide, &mut psid) }.is_err() || psid.is_invalid() {
        return None;
    }
    // Inner closure so the LocalFree below runs on every return path.
    let lookup = || {
        let mut name_len: u32 = 0;
        let mut domain_len: u32 = 0;
        let mut sid_use = SidTypeUser;
        // First call sizes name + domain (fails with insufficient buffer).
        // SAFETY: null buffers with zero sizes is the documented probe form.
        unsafe {
            let _ = LookupAccountSidW(
                None,
                psid,
                None,
                &mut name_len,
                None,
                &mut domain_len,
                &mut sid_use,
            );
        }
        if name_len == 0 {
            return None;
        }
        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len.max(1) as usize];
        // SAFETY: buffers sized by the probe above; the call fills `name` and
        // writes the (NUL-excluded) char count back into `name_len`.
        unsafe {
            LookupAccountSidW(
                None,
                psid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut sid_use,
            )
        }
        .ok()?;
        let leaf = String::from_utf16_lossy(&name[..name_len as usize]);
        let leaf = leaf.trim_end_matches('\0');
        if leaf.is_empty() {
            None
        } else {
            Some(leaf.to_string())
        }
    };
    let result = lookup();
    // SAFETY: psid was LocalAlloc'd by ConvertStringSidToSidW above.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psid.0)));
    }
    result
}

#[cfg(not(target_os = "windows"))]
fn translate_sid(_sid: &str) -> Option<String> {
    None
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

/// One poll over every source. Takes ownership of the watermark map and SID
/// resolver and returns them updated, so the caller can run this under
/// `spawn_blocking` (the `EvtQuery`/LSA FFI is synchronous and shouldn't block
/// the async runtime) while the resolver's cache persists across polls.
fn poll_once(
    pc_id: &str,
    dir: &Path,
    mut watermarks: Watermarks,
    mut resolver: SidResolver,
) -> (Watermarks, SidResolver) {
    let since = Utc::now() - chrono::Duration::hours(BOOTSTRAP_HOURS);
    // Domain reachability is probed once per poll, not per SID.
    let ctx = domain_ctx();
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
            let payload = match shape(s, ev, |sid| resolver.resolve(sid, ctx)) {
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
    (watermarks, resolver)
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
    // The SID→name cache persists across polls so a recurring user is only
    // looked up once.
    let mut resolver = SidResolver::default();
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
        let res = std::mem::take(&mut resolver);
        match tokio::task::spawn_blocking(move || {
            let (wm, res) = poll_once(&pc, &dir, wm, res);
            save_watermarks(&wpath, &wm);
            (wm, res)
        })
        .await
        {
            Ok((wm, res)) => {
                watermarks = wm;
                resolver = res;
            }
            Err(e) => {
                // A panic in the blocking poll shouldn't kill the reader;
                // reload the watermark from disk and carry on next tick (the
                // resolver starts empty — it just re-warms).
                warn!(error = %e, "winlog: poll task failed");
                watermarks = load_watermarks(&watermark_path);
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

    /// Identity user-resolver for the shape tests that don't exercise SID
    /// translation (it leaves the SID as-is, the raw-fallback behaviour).
    fn raw_user(sid: &str) -> String {
        sid.to_string()
    }

    #[test]
    fn shape_logon_resolves_user_and_keeps_raw_sid() {
        let s = &SOURCES[0]; // 7001 logon
        // The shape a real Winlogon 7001 carries. This fixture used to be
        // `<Data>2</Data><Data>S-1-5-21-…</Data>` — unnamed, which Windows
        // does not emit for a manifest-based provider — so it agreed with the
        // implementation's wrong assumption and the pair could never fail
        // (#1210). Replaced rather than added to, for that reason.
        let ev = parse(
            "",
            "<EventData><Data Name='TSId'>2</Data>\
                        <Data Name='UserSid'>S-1-5-21-1-2-3-1001</Data></EventData>",
        );
        // A resolver that turns the SID into a name → `user` is the name, but
        // `sid` keeps the raw value for forensics.
        let Shaped::Emit(p) = shape(s, &ev, |sid| {
            assert_eq!(sid, "S-1-5-21-1-2-3-1001");
            "alice".to_string()
        }) else {
            panic!("expected emit")
        };
        assert_eq!(p["session_id"], json!(2));
        assert_eq!(p["sid"], json!("S-1-5-21-1-2-3-1001"));
        assert_eq!(p["user"], json!("alice"));
        // When the resolver can't translate it falls back to the raw SID.
        let Shaped::Emit(p) = shape(s, &ev, raw_user) else {
            panic!("expected emit")
        };
        assert_eq!(p["user"], json!("S-1-5-21-1-2-3-1001"));

        // No UserSid at all → user and sid are both null and the resolver is
        // never called. This is what EVERY logon looked like in the field
        // between v0.43.100 and this fix, which is the point of keeping it:
        // the assertion is unchanged, but it now describes a genuinely
        // degenerate record rather than the normal case.
        let ev_nosid = parse("", "<EventData><Data Name='TSId'>5</Data></EventData>");
        let Shaped::Emit(p) = shape(s, &ev_nosid, |_| {
            panic!("resolver must not run with no SID")
        }) else {
            panic!("expected emit")
        };
        assert_eq!(p["user"], json!(null));
        assert_eq!(p["sid"], json!(null));
        assert_eq!(p["session_id"], json!(5));
    }

    /// The positional path is a fallback for a classic provider, not the
    /// shape Windows sends for Winlogon. Pinned separately and labelled, so
    /// that reading it can't be mistaken for evidence about the real record.
    #[test]
    fn shape_logon_falls_back_to_positional_data() {
        let s = &SOURCES[0]; // 7001 logon
        let ev = parse(
            "",
            "<EventData><Data>2</Data><Data>S-1-5-21-1-2-3-1001</Data></EventData>",
        );
        let Shaped::Emit(p) = shape(s, &ev, raw_user) else {
            panic!("expected emit")
        };
        assert_eq!(p["session_id"], json!(2));
        assert_eq!(p["sid"], json!("S-1-5-21-1-2-3-1001"));
    }

    #[test]
    fn should_translate_gates_domain_sids_on_reachability() {
        let reachable = DomainCtx {
            domain_joined: true,
            domain_reachable: true,
        };
        let offline = DomainCtx {
            domain_joined: true,
            domain_reachable: false,
        };
        let workgroup = DomainCtx {
            domain_joined: false,
            domain_reachable: false,
        };

        // Well-known / built-in SIDs are always local → always translate.
        assert!(should_translate("S-1-5-18", offline));
        assert!(!is_account_sid("S-1-5-18"));

        // A domain account SID only translates when the DC is reachable.
        let dom = "S-1-5-21-9-9-9-1001";
        assert!(is_account_sid(dom));
        assert!(should_translate(dom, reachable));
        assert!(!should_translate(dom, offline));

        // On a workgroup box every account SID is local → always translate,
        // even though it's S-1-5-21-shaped.
        assert!(should_translate(dom, workgroup));
    }

    #[test]
    fn resolver_caches_hits_and_falls_back_to_raw() {
        let mut r = SidResolver::default();
        // A pre-seeded cache entry is returned without a lookup.
        r.cache.insert("S-1-5-21-1-2-3-1001".into(), "alice".into());
        let ctx = DomainCtx {
            domain_joined: true,
            domain_reachable: true,
        };
        assert_eq!(r.resolve("S-1-5-21-1-2-3-1001", ctx), "alice");
        // An uncached SID: translate_sid is a no-op off Windows (and unresolved
        // on it for this fake SID), so it falls back to the raw SID and is NOT
        // cached (so a later poll can retry).
        assert_eq!(r.resolve("S-1-5-21-7-7-7-2002", ctx), "S-1-5-21-7-7-7-2002");
        assert!(!r.cache.contains_key("S-1-5-21-7-7-7-2002"));
    }

    #[test]
    fn shape_lock_uses_named_fields() {
        let s = &SOURCES[2]; // 4800 lock
        let ev = parse(
            "",
            "<EventData><Data Name='TargetUserName'>bob</Data><Data Name='SessionId'>1</Data></EventData>",
        );
        let Shaped::Emit(p) = shape(s, &ev, raw_user) else {
            panic!("expected emit")
        };
        assert_eq!(p["user"], json!("bob"));
        assert_eq!(p["session_id"], json!(1));
    }

    #[test]
    fn shape_modern_standby_flags_payload() {
        let s = &SOURCES[9]; // 506 sleep (modern standby)
        let ev = parse("", "");
        let Shaped::Emit(p) = shape(s, &ev, raw_user) else {
            panic!("expected emit")
        };
        assert_eq!(p, json!({ "standby": "modern" }));
    }

    #[test]
    fn shape_kernel_boot_27_only_emits_hibernate_resume() {
        let s = &SOURCES[11]; // 27 resume (hibernate gate)
        // BootType 0x2 → resume from hibernate. Named, as the real record is;
        // the old unnamed fixture is why this arm could fall through to
        // `Skip` on every host and no test noticed (#1210). `LoadOptions`
        // rides along in the real payload and is ignored.
        let hib = parse(
            "",
            "<EventData><Data Name='BootType'>2</Data>\
                        <Data Name='LoadOptions'></Data></EventData>",
        );
        let Shaped::Emit(p) = shape(s, &hib, raw_user) else {
            panic!("expected emit")
        };
        assert_eq!(p, json!({ "from": "hibernate" }));
        // BootType 0x0 (cold) / 0x1 (fast startup) → skip (covered by `boot`).
        assert!(matches!(
            shape(
                s,
                &parse("", "<EventData><Data Name='BootType'>0</Data></EventData>"),
                raw_user
            ),
            Shaped::Skip
        ));
        assert!(matches!(
            shape(
                s,
                &parse("", "<EventData><Data Name='BootType'>1</Data></EventData>"),
                raw_user
            ),
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
        let Shaped::Emit(p) = shape(s, &ev, raw_user) else {
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
        let Shaped::Emit(p) = shape(s, &parse("", ""), raw_user) else {
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
