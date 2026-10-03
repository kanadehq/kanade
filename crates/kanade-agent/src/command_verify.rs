//! Agent-side command provenance check (#1165).
//!
//! Verifies the backend's signature on every wire `Command`, reports the
//! outcome, and — on a host whose local config asks for it — **refuses** the
//! ones that do not verify.
//!
//! Refusing is off unless an operator turns it on, per host. That ordering was
//! deliberate rather than incidental: capability first, enforcement last, so
//! every step of the rollout is reversible (the same shape #1159 used for
//! per-role NATS credentials). Reporting shipped a release ahead of refusing,
//! which is how the fleet's readiness became answerable — via `command_keys`
//! on the heartbeat (#1195) — before anything depended on it.
//!
//! # Both entry points, not just the obvious one
//!
//! A wire `Command` reaches execution through two decode sites:
//!
//! * the live core subscription (`commands::command_loop`), and
//! * the JetStream replay on reconnect (`command_replay`).
//!
//! Both are wired here. The replay path matters more than it looks: #1155's
//! measured bypass *is* a JetStream consumer, so a verifier covering only the
//! live path would leave the attack it exists to stop running through the
//! other door.
//!
//! # Where enforcement will be configured — deliberately not KV
//!
//! When stage 3 adds "reject unsigned", the switch must live in **local
//! configuration** (registry / on-disk config, admin-ACL'd), not in the
//! `agent_config` KV bucket.
//!
//! KV is the convenient place — fleet-wide, instantly reversible per PC, which
//! is exactly what the rollout wants. It is also *inside the trust boundary
//! this feature defends*: today any holder of the shared NATS token can write
//! it (#1155), so an attacker who can forge commands could first flip the flag
//! that would have caught them. An enforcement switch reachable by the
//! attacker is not a switch.
//!
//! If a KV knob is wanted later for operational convenience, it may only
//! **raise** strictness, never lower it: local config sets the floor. Then the
//! worst an attacker gains from it is denial of service, not a bypass.
//!
//! # Keys come from the registry, not the binary
//!
//! The keyring is provisioned like the NATS token —
//! `HKLM\SOFTWARE\kanade\agent\CommandKeys`, hardened ACL — rather than baked
//! into the release. Baking one key in would be simpler, but a keyring has to
//! gain and retire entries during rotation, and re-releasing the fleet to
//! rotate a key is the kind of procedure that does not get used. The public
//! keys are not secrets; the ACL is there to stop tampering, not disclosure.
//!
//! Off-Windows there is no registry, so the same ring comes from
//! `/etc/kanade/command-keys.json` (root-only, 0600) when the registry yields
//! nothing, and enforcement from the root-only `/etc/kanade/require-signed-commands`
//! file. Precedence is registry, then file, then none; a registry that fails to
//! read does not fall through to the file. Both files are local config in the
//! same sense as the registry values — not KV — and both are read through the
//! same loader, so the reload and heartbeat behaviour below applies unchanged.
//!
//! # The ring reloads itself when, and only when, it is wrong
//!
//! Loading once at startup made provisioning a no-op until the agent
//! restarted — measured on the dev host, where the key was distributed
//! successfully at 17:27 and the agent, running since 15:47, still reported an
//! unknown key hours later. That defeats the whole "distribute before the
//! backend signs" ordering the rollout depends on: the key is on disk, the
//! in-memory ring is empty, and stage 2 raises the rotation alarm fleet-wide
//! anyway.
//!
//! So a [`VerifyError::UnknownKid`] triggers a reload and one retry. That is
//! the *only* outcome a stale ring can explain — an unsigned command, a bad
//! signature or a stale one are all unaffected by which keys we hold — so the
//! common path never touches the registry, and no other outcome can be used to
//! provoke a read.
//!
//! ## The command path alone is not enough — revocation has no trigger
//!
//! That narrowness is right for **verification**, and it was incomplete as a
//! policy for the ring, because verification is not the ring's only reader.
//! `UnknownKid` means "a key is missing". Revocation is the opposite shape: the
//! key is still *present* in memory, so every command signed with it verifies,
//! no unknown-key outcome ever occurs, and nothing asks the store again.
//! Removing a key from `CommandKeys` would have had no effect until the agent
//! restarted — which makes "the array is replaced, not merged, so emergency
//! revocation is possible" untrue in practice.
//!
//! The reported ring has the same gap in the other direction: a newly *added*
//! key stays invisible to the command path until something signs with it, so a
//! fleet-wide "has the new key landed everywhere?" check would answer no
//! indefinitely.
//!
//! So [`Verifier::refresh_and_report`] re-reads once per heartbeat, and the
//! heartbeat interval is what bounds revocation latency. It is cheap because
//! the raw value is compared before anything is parsed — the expensive part is
//! `VerifyingKey::from_bytes` (an Ed25519 point decompression per entry), and
//! an unchanged store skips it entirely.
//!
//! Chosen over a registry change notification, which would react in
//! sub-second rather than sub-interval, for one reason: **a watcher that
//! silently died would leave revocation looking instant while it was not**.
//! Refreshing on the same path that *reports* the ring means one signal covers
//! both — if the refresh stops working, `command_keys` visibly stops matching
//! what was provisioned. A faster mechanism can be added later on top of this
//! one; it should not replace it.
//!
//! The reload is rate-limited because the trigger is reachable by anyone who
//! can put bytes on a command subject: without a floor, a stream of commands
//! bearing invented key ids would turn every one of them into a registry read.
//! [`RELOAD_MIN_INTERVAL`] bounds that to one read per interval per machine,
//! which still lets a freshly provisioned key take effect within seconds
//! rather than at the next restart.
//!
//! ## A reload may improve the ring or leave it alone — never destroy it
//!
//! This is what makes a skipped or failed reload a *delay* rather than an
//! outage, and it is load-bearing rather than tidy. The provisioning job writes
//! `CommandKeys` while the agent is running, so a reload can catch a partial
//! write; treating that like an absent value — which is what a boot-time load
//! correctly does — would take a machine from verifying to holding no keys at
//! all. At stage 3 that is the difference between one delayed command and a
//! machine that refuses every command until something reloads successfully.
//!
//! So [`read_keyring_raw`] separates "nothing provisioned" (`Ok(None)` — a real
//! state an operator can intend) from "provisioned and unusable" (`Err`), and
//! only the former replaces a live ring.
//!
//! ## An enforcing host ignores the rate limit
//!
//! With enforcement on, the moment a command is about to be **refused** for an
//! unknown key is exactly the moment a skipped reload stops being free: a key
//! that landed twenty seconds ago would be refused for the rest of the window,
//! and the limit would have turned from an I/O bound into a rejection bug. So
//! [`Verifier::classify`] forces the reload when this host is enforcing,
//! ignoring [`RELOAD_MIN_INTERVAL`].
//!
//! The flooding that bound existed to stop is not worth a rejection here. The
//! read is a local, memory-mapped registry lookup — cheaper than the Ed25519
//! verify the same command already cost — so an attacker naming invented key
//! ids buys microseconds per command they were already paying for.
//!
//! ## If the keyring source stops being local
//!
//! The reload runs synchronously on the async command path, which is fine only
//! because the registry and the keyring file are local: microseconds, at most
//! once per interval per machine. Move the ring to a network fetch, a remote share
//! or a KV read and that inverts — it then belongs on `spawn_blocking`, and the
//! `last_reload` guard held across the load (which is what collapses two
//! concurrent misses into one read) has to be reworked around an async-aware
//! lock rather than simply dropped, or the dedup it provides is lost.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use kanade_shared::signing::{KeyPolicy, KeyRing, SigHeaders, VerifyError, verify};
use kanade_shared::wire::{Command, ENVELOPE_KIND_V2, EnvelopeError, ObsEvent, verify_envelope};
use serde::Deserialize;
use tracing::{error, info, warn};

const REG_SUBKEY: &str = r"SOFTWARE\kanade\agent";
const REG_VALUE: &str = "CommandKeys";
/// Registry value that turns stage 3 on for this host: `"1"` / `"true"`.
///
/// **Local config, never KV** — the reasoning is in the module doc above. Read
/// once at construction: flipping enforcement is a deliberate act that should
/// take effect at a moment an operator chose, and a value that could change
/// under a running agent would make "was this host enforcing when it refused?"
/// unanswerable after the fact.
const REG_ENFORCE: &str = "RequireSignedCommands";

/// File-based keyring for hosts with no registry, the off-Windows counterpart
/// of `CommandKeys` — same JSON array, same strict [`parse_keyring`] rules.
///
/// Provisioned like the NATS token in `agent.env`: a root-owned 0600 file under
/// `/etc/kanade`, written by the setup script. The public keys are not secret;
/// the mode is there to stop tampering, exactly as the registry ACL is.
const KEYRING_FILE: &str = "/etc/kanade/command-keys.json";

/// File that turns enforcement on off-Windows — the counterpart of
/// [`REG_ENFORCE`], and like it **local config, never KV**. Holds `1` / `true`
/// / `yes`; anything else, or no file, leaves enforcement off. Its own file
/// rather than a line in `agent.env` because launchd only exports the token
/// from `agent.env`, so a setting there would silently not reach a macOS agent.
const ENFORCE_FILE: &str = "/etc/kanade/require-signed-commands";

/// Where an operator provisions the keyring on this platform, for messages.
#[cfg(windows)]
macro_rules! keyring_location {
    () => {
        r"HKLM\SOFTWARE\kanade\agent\CommandKeys"
    };
}
#[cfg(not(windows))]
macro_rules! keyring_location {
    () => {
        "/etc/kanade/command-keys.json"
    };
}

/// `source` on emitted [`ObsEvent`]s.
const SOURCE: &str = "command_signature";

/// Floor between two keyring reloads.
///
/// The reload trigger is attacker-reachable — anyone who can place bytes on a
/// command subject can name a key id we do not hold — so this is what stops
/// that from becoming one registry read per delivered command. Short enough
/// that a newly provisioned key takes effect on the next command rather than
/// at the next restart, which is the whole point of reloading at all.
const RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Floor an **enforcing** host uses instead, when it is about to refuse.
///
/// Short, but not zero. Removing the floor entirely made every unknown-kid
/// command do a registry read *while holding `last_reload`*, so concurrent
/// command paths serialize behind one I/O each — for traffic whose `kid` an
/// attacker picks. The cost that matters there is the serialization, not the
/// microseconds.
///
/// A second still lets a key that landed moments ago be picked up: the worst
/// case is that one command is refused and the next one succeeds, which is
/// bounded and self-correcting. Thirty seconds was not — it was long enough
/// that an operator watching a rotation would see refusals and conclude the
/// key had not landed.
const RELOAD_FORCED_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// One entry of the JSON array stored in the registry.
#[derive(Debug, Deserialize)]
struct KeyEntry {
    kid: String,
    /// Base64 (standard) 32-byte Ed25519 public key.
    public_key: String,
    #[serde(default)]
    label: Option<String>,
    /// Present for a break-glass key; absent for the ordinary signer.
    #[serde(default)]
    max_age_secs: Option<u64>,
    #[serde(default)]
    audit_every_use: bool,
}

/// Read the trusted keys, distinguishing "nothing is provisioned" from
/// "something is provisioned and it is broken".
///
/// The split exists because the two answers are only interchangeable at boot.
/// An **absent** value is a legitimate state — no keys yet, or an operator
/// deliberately revoking the ring — and yields an empty ring. An **unparseable**
/// value is a failure, and as a reload it must not be allowed to replace a ring
/// that is currently working: the provisioning job writes this value while the
/// agent is running, so a reload can catch a partial write, and collapsing that
/// into "empty" would take a machine from verifying to holding nothing. At
/// stage 3 that is the difference between one skipped reload and a machine that
/// rejects every command.
/// Whether local config asks this host to enforce (#1165 stage 3).
///
/// Deliberately reads only local config: the registry on Windows, a root-only
/// file elsewhere. The `agent_config` KV bucket is the convenient place and the
/// wrong one: any holder of the shared NATS token can write it (#1155), so an
/// attacker able to forge commands could first turn off the check that would
/// have caught them. An enforcement switch reachable by the attacker is not a
/// switch.
fn enforce_requested() -> bool {
    enforce_requested_from(
        kanade_shared::secrets::read_hklm_value(REG_SUBKEY, REG_ENFORCE),
        platform_file(ENFORCE_FILE)
            .as_deref()
            .map(|p| (p, ROOT_UID)),
    )
}

/// The file store's path on this platform: `None` on Windows, where the
/// registry is the only store and a stray file must not become a second one.
fn platform_file(path: &str) -> Option<std::path::PathBuf> {
    (!cfg!(windows)).then(|| std::path::PathBuf::from(path))
}

/// Pure core of [`enforce_requested`], with both stores injected (the file as
/// `(path, owner uid)`). The registry wins when it is set; the file is
/// consulted only when the registry has nothing.
///
/// A file that cannot be read for an ordinary reason reads as "not requested" —
/// the host keeps verifying and reporting but does not refuse, the same
/// fail-open the registry read has always had. A file that exists but is owned
/// by someone other than `owner`, or is writable by group/other, is the
/// opposite case: whoever can write it could flip `1` to `0`, and ignoring it
/// would hand them exactly that. So an untrusted file is treated as a request
/// to enforce whatever it says; the worst that costs is refusals on a host
/// whose ring is non-empty (an empty ring still declines), and only root can
/// put the host back.
fn enforce_requested_from(registry: Option<String>, file: Option<(&std::path::Path, u32)>) -> bool {
    let value = registry.or_else(|| {
        let (path, owner) = file?;
        if let Err(e) = check_file_trusted(path, owner) {
            error!(error = %e, "enforcement file is not root-only — treating enforcement as requested");
            return Some("1".to_string());
        }
        std::fs::read_to_string(path).ok()
    });
    matches!(
        value
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes")
    )
}

/// The kids on a ring, for a log line. Free function so it can be called
/// before the ring moves into the struct.
fn lock_kids(ring: &KeyRing) -> Vec<&str> {
    ring.kids().collect()
}

fn read_keyring_raw() -> Result<Option<String>, String> {
    // `try_read_hklm_value`, not `read_hklm_value`: the latter reports a failed
    // read as `None`, indistinguishable from a value that is genuinely absent.
    // Read once at startup that hardly matters. Read on a schedule it matters a
    // lot — "absent" means *adopt an empty ring*, so a transient failure would
    // silently drop every key this machine trusts, and keep doing it.
    read_keyring_raw_from(
        kanade_shared::secrets::try_read_hklm_value(REG_SUBKEY, REG_VALUE),
        platform_file(KEYRING_FILE)
            .as_deref()
            .map(|p| (p, ROOT_UID)),
    )
}

/// The only owner the keyring file is trusted to have. Injected into the reader
/// (as `(path, owner uid)`) so tests, which cannot create root-owned files, can
/// name their own uid instead.
const ROOT_UID: u32 = 0;

/// Combine the two stores: registry, then file, then nothing.
///
/// A registry that holds a value wins outright. A registry that *failed* is an
/// error and does not fall through to the file — "could not read the
/// authoritative store" must not quietly become "use the other one", or a
/// transient registry failure would swap in a possibly older ring. Only a
/// registry with nothing in it defers to the file.
fn read_keyring_raw_from(
    registry: Result<Option<String>, String>,
    file: Option<(&std::path::Path, u32)>,
) -> Result<Option<String>, String> {
    match registry? {
        Some(v) => Ok(Some(v)),
        None => match file {
            Some((p, owner)) => read_keyring_file(p, owner),
            None => Ok(None),
        },
    }
}

/// Refuse a local-config file another account could rewrite. The mode alone is
/// not enough: a 0600 file owned by some other account is writable by that
/// account, so the owner is checked too. A file that does not exist passes —
/// absence is the caller's to interpret. No-op off unix.
fn check_file_trusted(path: &std::path::Path, owner: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Ok(m) = std::fs::metadata(path) {
            if m.uid() != owner {
                return Err(format!(
                    "{}: owned by uid {}, not uid {owner} — refusing to trust it; chown root",
                    path.display(),
                    m.uid()
                ));
            }
            if m.permissions().mode() & 0o022 != 0 {
                return Err(format!(
                    "{}: writable by group/other (mode {:o}) — refusing to trust it; chmod 0600",
                    path.display(),
                    m.permissions().mode() & 0o777
                ));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (path, owner);
    Ok(())
}

/// Read the file store, keeping "absent" distinct from "present and unusable"
/// for the same reason the registry does: absent adopts an empty ring, anything
/// else must not be allowed to replace a working one.
fn read_keyring_file(path: &std::path::Path, owner: u32) -> Result<Option<String>, String> {
    check_file_trusted(path, owner)?;
    interpret_file_read(path, std::fs::read_to_string(path))
}

/// Classify the outcome of reading the file. Split from the read so a
/// permission failure is reachable from a test on any OS, including as root.
fn interpret_file_read(
    path: &std::path::Path,
    read: std::io::Result<String>,
) -> Result<Option<String>, String> {
    match read {
        Ok(s) if s.trim().is_empty() => Err(format!(
            "{}: file is empty — write `[]` to revoke every key deliberately, or remove the file",
            path.display()
        )),
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Parse what [`read_keyring_raw`] returned. `None` (value absent) is the
/// legitimate "nothing provisioned, or an operator revoked everything" state
/// and yields an empty ring; an unparseable value is an error the caller must
/// not let replace a working ring.
fn parse_raw(raw: Option<&str>) -> Result<KeyRing, String> {
    match raw {
        None => Ok(KeyRing::new()),
        Some(s) => parse_keyring(s),
    }
}

fn parse_keyring(raw: &str) -> Result<KeyRing, String> {
    use base64::Engine;
    let entries: Vec<KeyEntry> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let mut ring = KeyRing::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for e in entries {
        // A ring is keyed by `kid`, so a repeat would have one entry silently
        // replace the other and every command signed by the loser would stop
        // verifying with nothing to explain it. That is the worst available
        // outcome: the array is what an operator hand-assembles or re-types
        // during an incident, and "two different keys under one id" is exactly
        // the state the whole scheme assumes cannot happen. Refuse the ring
        // instead — loudly, in the same way a malformed entry does, and for the
        // same reason (half a keyring is worse than none).
        if !seen.insert(e.kid.clone()) {
            return Err(format!(
                "key {} appears twice — two different keys must never share an id, and a ring \
                 keyed by id cannot hold both",
                e.kid
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&e.public_key)
            .map_err(|err| format!("key {}: {err}", e.kid))?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| format!("key {}: expected 32 bytes, got {}", e.kid, bytes.len()))?;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&arr)
            .map_err(|err| format!("key {}: {err}", e.kid))?;
        let label = e.label.unwrap_or_else(|| e.kid.clone());
        let policy = match e.max_age_secs {
            Some(secs) => KeyPolicy::break_glass(label, std::time::Duration::from_secs(secs)),
            None => {
                let mut p = KeyPolicy::backend(label);
                p.audit_every_use = e.audit_every_use;
                p
            }
        };
        ring.insert(e.kid, vk, policy);
    }
    Ok(ring)
}

/// Validate a keyring file the way the Windows deploy script validates
/// `-CommandKeys`, plus the agent's own strict parse, and return its kids.
///
/// Backs the agent's hidden `--check-command-keys` flag, which the setup
/// scripts call before writing anything. Using the agent's own parser means the
/// installer cannot accept a ring the running agent would then reject, and it
/// needs no `jq` on a minimal Linux or a stock macOS.
pub fn check_keyring_file(path: &std::path::Path) -> Result<Vec<String>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|e| format!("not valid JSON: {e}"))?;
    let entries = value
        .as_array()
        .ok_or("must be a JSON ARRAY of entries, even for a single key")?;
    if entries.is_empty() {
        return Err("is an empty array — that provisions no keys at all".into());
    }
    let mut kids: Vec<String> = Vec::new();
    for e in entries {
        let field = |name: &str| {
            e.get(name)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
        };
        let (Some(kid), Some(_)) = (field("kid"), field("public_key")) else {
            return Err(format!(
                "every entry needs a non-empty STRING 'kid' and 'public_key'; got: {e}"
            ));
        };
        if e.get("label").is_some_and(|v| !v.is_string()) {
            return Err(format!("key {kid}: 'label' must be a string"));
        }
        // `max_age_secs` is what makes an entry break-glass, so a quoted value
        // would silently demote it to an unbounded key, and zero would make
        // `verify` reject every signature that is not exactly zero seconds
        // old: the entry looks provisioned and fails when it is reached for.
        if let Some(v) = e.get("max_age_secs") {
            match v.as_u64() {
                Some(n) if n > 0 => {}
                _ => {
                    return Err(format!(
                        "key {kid}: 'max_age_secs' must be a positive integer (do not quote it); \
                         a zero window rejects every signature made with the key"
                    ));
                }
            }
        }
        if kids.iter().any(|k| k == kid) {
            return Err(format!(
                "key id {kid} appears more than once — two different keys must never share an id"
            ));
        }
        kids.push(kid.to_string());
    }
    parse_keyring(raw.trim())?;
    Ok(kids)
}

/// Pull the signature headers off a NATS message.
pub fn headers_of(msg: &async_nats::Message) -> SigHeaders {
    let get = |name: &str| {
        msg.headers
            .as_ref()
            .and_then(|h| h.get(name))
            .map(|v| v.to_string())
    };
    SigHeaders {
        sig_b64: get(kanade_shared::signing::SIG),
        kid: get(kanade_shared::signing::SIG_KID),
        alg: get(kanade_shared::signing::SIG_ALG),
        at_ms: get(kanade_shared::signing::SIG_AT),
    }
}

/// The coarse class an outcome falls into, which is what gets reported.
///
/// Coarser than [`VerifyError`] on purpose: the reported signal is a state the
/// fleet is *in*, and per-command detail belongs in the log line, not in a
/// timeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Signed by a key on this agent's ring, and the bytes match.
    Verified,
    /// No signature at all — normal traffic until the backend starts signing.
    Unsigned,
    /// Signed, and this agent holds **no keys at all** — provisioning has not
    /// reached it, or what reached it failed to parse.
    ///
    /// Split from [`Outcome::UnknownKid`] because the two are different
    /// operational states with different fixes, and conflating them makes the
    /// rollout unreadable: during stages 1-2 every not-yet-provisioned machine
    /// would raise the *rotation* alarm, which is supposed to mean "this
    /// machine missed a key change". An alarm that fires on the normal
    /// starting state is one operators learn to ignore before it ever matters.
    Unprovisioned,
    /// Signed by a key this agent does not have, **while holding others**.
    /// A stale keyring mid-rotation: provisioning reached this machine once,
    /// but not for this key.
    UnknownKid,
    /// Signed, and the signature does not check out. Either a forgery or a
    /// corrupted message; both warrant a look.
    Invalid,
    /// Genuine, but older than its key's policy allows. Reported separately
    /// from `Invalid` because nothing is wrong with the message — a replayed
    /// break-glass command and a forgery need different responses.
    Stale,
    /// A v2 envelope signed further in the future than the clock allowance:
    /// the signer's clock and this host's disagree. Reported apart from
    /// [`Outcome::Invalid`] so an operator can tell a clock problem from
    /// tampering — the signature is genuine.
    ClockAhead,
    /// A genuine v2 envelope whose start deadline (or its key's age bound) has
    /// passed. Also a clock-shaped refusal: a host with a clock running ahead
    /// sees live envelopes as expired.
    Expired,
    /// A genuine v2 envelope for a different host.
    Misaddressed,
    /// A genuine v2 envelope that is structurally unacceptable: no recipient,
    /// no or malformed expiry, a reversed range, a window over the ceiling, or
    /// an unreadable body.
    EnvelopeRejected,
}

impl Outcome {
    /// Whether a host that is enforcing should refuse to run this command.
    ///
    /// `Verified` runs. Everything else — including [`Outcome::Unsigned`],
    /// which is the whole point of stage 3 — does not.
    ///
    /// A method on the outcome rather than a `match` at the call site so the
    /// answer cannot drift between the live subscription and the JetStream
    /// replay. Those are two decode paths for the same bytes (#1155's measured
    /// bypass *is* a consumer), and an enforcement gate that covered only one
    /// would leave the attack it exists to stop running through the other.
    pub fn is_refusal(self) -> bool {
        !matches!(self, Outcome::Verified)
    }

    /// The stderr an operator reads when their command was refused.
    ///
    /// Names the state rather than restating the code, because the person
    /// reading it is mid-incident and the useful content is what to do next.
    fn refusal_reason(self) -> &'static str {
        match self {
            Outcome::Verified => "verified",
            Outcome::Unsigned => {
                "command carried no signature, and this host requires one. If this came from \
                 `kanade run`, set the break-glass key; if from the backend, that backend is not \
                 signing yet."
            }
            Outcome::Unprovisioned => {
                concat!(
                    "this host holds no command-signing keys at all, so nothing can be verified. \
                     Provision ",
                    keyring_location!(),
                    "."
                )
            }
            Outcome::UnknownKid => {
                "signed by a key this host does not have — it likely missed a rotation. Re-run \
                 the keyring provisioning for this machine."
            }
            Outcome::Invalid => {
                "signature does not match these bytes. Either the command was tampered with in \
                 flight or it was signed by a key that is not the one it claims."
            }
            Outcome::Stale => {
                "signature is outside its freshness window. Most often the clocks disagree — \
                 compare this host's time with the signing host's before assuming a replay."
            }
            Outcome::ClockAhead => {
                "envelope was signed further in the future than this host's clock allowance. \
                 This host's clock is probably behind the signer's — check its time before \
                 assuming tampering."
            }
            Outcome::Expired => {
                "envelope's start deadline has passed. Re-issue the command; if it was issued \
                 moments ago, this host's clock is probably ahead of the signer's."
            }
            Outcome::Misaddressed => {
                "envelope names a different host as its recipient, so this host will not run \
                 it. A captured command re-sent here is refused by design."
            }
            Outcome::EnvelopeRejected => {
                "envelope is malformed or outside the accepted validity window (no recipient or \
                 expiry, reversed range, or longer than the ceiling). Re-issue it from the \
                 current backend key."
            }
        }
    }

    /// Every variant, kept **here** rather than in the test that consumes it.
    ///
    /// The uniqueness guard on [`Outcome::kind`] is only as good as this list,
    /// and a list living in a test module is one a new variant gets added
    /// without — which is exactly what happened when `Unprovisioned` was added.
    /// Sitting against the enum, it is in the diff you are already editing.
    #[cfg(test)]
    const ALL: [Outcome; 10] = [
        Outcome::Verified,
        Outcome::Unsigned,
        Outcome::Unprovisioned,
        Outcome::UnknownKid,
        Outcome::Invalid,
        Outcome::Stale,
        Outcome::ClockAhead,
        Outcome::Expired,
        Outcome::Misaddressed,
        Outcome::EnvelopeRejected,
    ];

    fn kind(self) -> &'static str {
        match self {
            Outcome::Verified => "command_signature_ok",
            Outcome::Unsigned => "command_signature_absent",
            Outcome::Unprovisioned => "command_signature_unprovisioned",
            Outcome::UnknownKid => "command_signature_unknown_key",
            Outcome::Invalid => "command_signature_invalid",
            Outcome::Stale => "command_signature_stale",
            Outcome::ClockAhead => "command_signature_clock_ahead",
            Outcome::Expired => "command_signature_expired",
            Outcome::Misaddressed => "command_signature_misaddressed",
            Outcome::EnvelopeRejected => "command_signature_envelope_rejected",
        }
    }
}

/// What a reload attempt did, for the log line an operator reads when a
/// machine will not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reload {
    /// The ring was replaced with what the store holds.
    Done,
    /// We read the store and it is byte-identical to what the ring was built
    /// from, so nothing was parsed. Distinct from [`Reload::Done`] because a
    /// retry after this cannot succeed — the ring did not move.
    Unchanged,
    /// Inside [`RELOAD_MIN_INTERVAL`] of the last attempt — we did not look.
    RateLimited,
    /// We looked and could not use what we found. The previous ring is kept.
    Failed,
}

impl Reload {
    fn as_str(self) -> &'static str {
        match self {
            Reload::Done => "reloaded",
            Reload::Unchanged => "unchanged",
            Reload::RateLimited => "rate-limited",
            Reload::Failed => "reload-failed",
        }
    }
}

/// Take a lock, ignoring poisoning.
///
/// A panic on some other command path must not stop this machine verifying
/// the next one: every value behind these locks is replaceable state
/// (a keyring, a timestamp, a last-reported class), so the worst a poisoned
/// guard can carry is a stale value that the next call overwrites anyway.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How the ring is (re)loaded. Boxed rather than hard-wired to
/// [`read_keyring_raw`] so the reload path is reachable from a test — the registry
/// returns nothing on non-Windows, so a test that went through it would assert
/// nothing at all on CI, which is where this needs to hold.
/// Yields the store's **raw** value: `Ok(None)` = absent (a state an operator
/// can intend), `Ok(Some(s))` = the JSON as stored, `Err` = unreadable.
///
/// Raw rather than parsed so the refresh can compare bytes and skip the work
/// when nothing changed. The expensive part is not the registry read — it is
/// `VerifyingKey::from_bytes`, which decompresses an Ed25519 point per entry
/// (tens of microseconds each). Comparing first makes the steady state a
/// single memory-mapped read.
type Loader = Box<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;

/// Verifies commands and reports when the fleet's signing state changes.
pub struct Verifier {
    /// Behind a lock because an `UnknownKid` replaces it in place — see the
    /// module doc. `Mutex` rather than `RwLock`: reads are already serialized
    /// by the command path being one message at a time per subscription, and
    /// the lock is held for a signature verify (tens of microseconds).
    ring: Mutex<KeyRing>,
    /// The raw store value [`Verifier::ring`] was built from, so a refresh can
    /// skip parsing when nothing changed. `None` = the value was absent.
    last_raw: Mutex<Option<String>>,
    loader: Loader,
    /// What local config asked for (#1165 stage 3) — **not** the answer.
    ///
    /// Whether this host actually refuses is [`Verifier::enforcing_now`],
    /// which also consults the live ring. Config is fixed for the process;
    /// the ring is not, and conflating them once cost a bricking bug: an
    /// enforcing host whose ring later reloaded to empty would have refused
    /// every command, including the one that would restore its keys.
    enforce_requested: bool,
    /// Set once we have warned about declining to enforce on an empty ring, so
    /// the warning marks the transition rather than repeating per command.
    empty_ring_warned: std::sync::atomic::AtomicBool,
    /// When the ring was last pulled from the store, for [`RELOAD_MIN_INTERVAL`].
    last_reload: Mutex<Instant>,
    pc_id: String,
    obs_dir: std::path::PathBuf,
    /// Last reported class. Events fire on **transition**, the same shape the
    /// idle sampler uses: a per-command event would emit thousands of
    /// "unsigned" rows a day through stages 1-2 and bury the one that matters.
    last: Mutex<Option<Outcome>>,
}

impl Verifier {
    /// Production constructor: loads the ring from the registry now, and
    /// reloads from the same place when a command names a key it lacks.
    pub fn new(pc_id: String, obs_dir: std::path::PathBuf) -> Self {
        Self::with_loader_and_policy(
            pc_id,
            obs_dir,
            Box::new(read_keyring_raw),
            enforce_requested(),
        )
    }

    /// The ring's source is a single argument so the initial load and every
    /// reload cannot drift apart — the caller can no longer hand in one ring
    /// and have it silently refreshed from somewhere else.
    #[cfg(test)]
    fn with_loader(pc_id: String, obs_dir: std::path::PathBuf, loader: Loader) -> Self {
        Self::with_loader_and_policy(pc_id, obs_dir, loader, false)
    }

    fn with_loader_and_policy(
        pc_id: String,
        obs_dir: std::path::PathBuf,
        loader: Loader,
        enforce_requested: bool,
    ) -> Self {
        // Every other `obs_outbox::enqueue` caller in this crate does this
        // first; skipping it would make the first report fail on a fresh
        // install, which is precisely when a mis-provisioned keyring is most
        // likely and least visible.
        if let Err(e) = crate::obs_outbox::ensure_outbox_dir(&obs_dir) {
            warn!(error = %e, "command_verify: outbox dir — reports may be dropped until it exists");
        }
        // Two different failures, two different messages — they send an
        // operator to different places. One means the store could not be read
        // at all (permissions, a missing hive); the other means it was read and
        // holds something that is not a keyring.
        let raw = loader().unwrap_or_else(|e| {
            warn!(error = %e, "command keyring could not be READ — starting with no keys");
            None
        });
        let ring = parse_raw(raw.as_deref()).unwrap_or_else(|e| {
            // No earlier ring to lose at construction, so degrading is safe
            // here in a way it is not on the refresh path.
            warn!(error = %e, "command keyring is present but UNPARSEABLE — starting with no keys");
            KeyRing::new()
        });
        if ring.is_empty() {
            info!(
                "command keyring is empty — signed commands will be reported unprovisioned until \
                 one is distributed (no restart needed; the ring reloads on demand)"
            );
        } else {
            info!(kids = ?ring.kids().collect::<Vec<_>>(), "command keyring loaded");
        }
        // An empty ring cannot enforce. Nothing verifies against no keys, so
        // "enforce" there means "refuse every command", which is not a
        // security posture — it is a machine that has stopped working, and one
        // that cannot be fixed remotely because the command carrying the keys
        // is refused along with the rest.
        //
        // Declining costs nothing an attacker can use: emptying the ring needs
        // local administrator on this host, and someone with that can already
        // run anything here — they gain no reach they did not have. What it
        // buys is turning a bricked endpoint into a visible misconfiguration,
        // and a safety net under the class of bug that nearly shipped in
        // #1186, where a failed reload wiped a working ring.
        let enforcing = enforce_requested && !ring.is_empty();
        if enforce_requested && !enforcing {
            error!(
                "RequireSignedCommands is set but this host holds NO command-signing keys — \
                 refusing to enforce, because that would reject every command including the one \
                 that would provision the keys. Provision the keyring; the ring reloads on \
                 demand, so no restart is needed."
            );
        } else if enforcing {
            warn!(
                kids = ?lock_kids(&ring),
                "enforcing command signatures — unverified commands will be REFUSED"
            );
        }
        Self {
            ring: Mutex::new(ring),
            last_raw: Mutex::new(raw),
            loader,
            enforce_requested,
            empty_ring_warned: std::sync::atomic::AtomicBool::new(false),
            last_reload: Mutex::new(Instant::now()),
            pc_id,
            obs_dir,
            last: Mutex::new(None),
        }
    }

    /// Whether this host refuses right now.
    ///
    /// Evaluated per decision, **not** cached from construction, because the
    /// ring can change under a running agent. `read_keyring` maps an absent
    /// registry value to `Ok(empty)` — a state an operator can legitimately
    /// intend, by revoking every key — and a provisioning script that deletes
    /// before writing passes through it. If enforcement were a boot-time
    /// snapshot, a host that reloaded into an empty ring would keep refusing
    /// with nothing to verify against: every command `Unprovisioned`, every
    /// command refused, including the one that would restore its keys. That is
    /// precisely the bricking the constructor check exists to prevent, reached
    /// through the reload path instead.
    fn enforcing_now(&self) -> bool {
        if !self.enforce_requested {
            return false;
        }
        if lock(&self.ring).is_empty() {
            // Warn on the transition, not per command: this fires from the
            // command path, and the state it describes persists until someone
            // acts on it. The per-command signal is the `Unprovisioned`
            // outcome, which is already reported and fleet-enumerable (#1195).
            if !self
                .empty_ring_warned
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                error!(concat!(
                    "this host is configured to require signed commands but its keyring is \
                         now EMPTY — declining to enforce rather than refusing everything, \
                         including the command that would restore the keys. Re-provision ",
                    keyring_location!(),
                    "."
                ));
            }
            return false;
        }
        self.empty_ring_warned
            .store(false, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// The same predicate as [`Verifier::enforcing_now`], against a ring the
    /// caller already holds — the reporting path (#1250).
    ///
    /// Split from the decision path for two reasons. It takes the lock as an
    /// argument, so the ring and the enforcement state reported on one
    /// heartbeat describe **one instant** rather than two reads a reload could
    /// slip between. And it does not warn: the empty-ring log is about
    /// declining to act, and firing it from an observation would make its
    /// volume a function of the heartbeat interval. Reporting `false` is the
    /// signal here, and unlike the log it is fleet-enumerable.
    fn enforcing_with(&self, ring: &KeyRing) -> bool {
        self.enforce_requested && !ring.is_empty()
    }

    /// The stderr for a refusal, or `None` when this outcome is allowed to
    /// run — either because it verified, or because this host is not
    /// enforcing.
    ///
    /// Returning the message rather than a bool keeps the "why" attached to
    /// the decision; the caller publishes it as the refusal's stderr, which is
    /// the only thing the issuer will see.
    pub fn refusal(&self, outcome: Outcome) -> Option<&'static str> {
        (outcome.is_refusal() && self.enforcing_now()).then(|| outcome.refusal_reason())
    }

    /// The ring **as it stands in memory**, as `kid:fingerprint`.
    ///
    /// Test-only since #1250 folded reporting into
    /// [`Verifier::refresh_and_report`], which reads the ring and the
    /// enforcement state under one lock. Kept because the reload tests need to
    /// inspect the ring *without* refreshing it — asserting what a specific
    /// `pull` left behind is the whole point there, and an accessor that
    /// re-read would answer a different question.
    ///
    /// Deliberately not a registry read. Those two diverge between a key
    /// landing on disk and the reload that picks it up, and the question an
    /// operator is asking — "would this machine accept a command signed by X
    /// right now" — is answered by memory. Reporting the file would describe a
    /// machine that does not exist yet, and at stage 3 that is the difference
    /// between "safe to retire the old key" and a stranded endpoint.
    ///
    /// The fingerprint is what makes the answer comparable *across* machines
    /// rather than only within one (#1229): the id alone is chosen by whoever
    /// wrote the ring, so two hosts can agree on it while holding different
    /// keys, and that host refuses every command with nothing in the fleet view
    /// to distinguish it.
    #[cfg(test)]
    pub fn trusted_keys(&self) -> Vec<String> {
        lock(&self.ring).kid_fingerprints().collect()
    }

    /// Check one legacy message and report the outcome, with the clock injected
    /// so the freshness branch is reachable from a test.
    ///
    /// **Classifies; does not decide.** Acting on the answer is
    /// [`Verifier::refusal`], and the split is what lets a non-enforcing host
    /// report exactly what an enforcing one would refuse. Production callers go
    /// through [`Verifier::admit`].
    #[cfg(test)]
    fn observe_at(
        &self,
        body: &[u8],
        headers: &SigHeaders,
        request_id: &str,
        now_ms: i64,
    ) -> Outcome {
        self.observe_at_instant(body, headers, request_id, now_ms, Instant::now())
    }

    fn observe_at_instant(
        &self,
        body: &[u8],
        headers: &SigHeaders,
        request_id: &str,
        now_ms: i64,
        instant: Instant,
    ) -> Outcome {
        let outcome = self.classify(body, headers, request_id, now_ms, instant);
        self.report_transition(outcome);
        outcome
    }

    /// Verify, reloading the ring once if the only thing wrong is that we do
    /// not hold the named key.
    ///
    /// `now` is passed in rather than read here so the rate limit is testable
    /// without sleeping.
    fn classify(
        &self,
        body: &[u8],
        headers: &SigHeaders,
        request_id: &str,
        now_ms: i64,
        now: Instant,
    ) -> Outcome {
        self.with_reload(request_id, now, || {
            self.check(body, headers, request_id, now_ms)
        })
        .unwrap_or_else(|missing| missing)
    }

    /// Run `check` against the ring, reloading it once if the only thing wrong
    /// is that we do not hold the named key. `Err` is the outcome for a key we
    /// still do not hold.
    ///
    /// Shared by the legacy and envelope paths so the reload rules — which are
    /// security-relevant (the trigger is reachable by anyone who can put bytes
    /// on a command subject) — exist once.
    fn with_reload<T>(
        &self,
        request_id: &str,
        now: Instant,
        check: impl Fn() -> Result<T, String>,
    ) -> Result<T, Outcome> {
        match check() {
            Ok(v) => Ok(v),
            // The one outcome a stale in-memory ring can explain. Everything
            // else — unsigned, malformed, bad signature, stale — means the same
            // thing whatever keys we hold, so it must not reach the store: the
            // trigger is reachable by anyone who can put bytes on a command
            // subject.
            Err(kid) => {
                // An enforcing host ignores the rate limit here. This is the
                // moment the limit stops being free: about to refuse for a key
                // we might already hold on disk, a skipped read turns an I/O
                // bound into a rejection bug, and a key provisioned twenty
                // seconds ago would be refused for the remainder of the
                // window. The read is a local, memory-mapped registry lookup —
                // cheaper than the Ed25519 verify this command already cost —
                // so the flooding an attacker could provoke is not
                // amplification worth a rejection.
                //
                // Keyed on the *config* rather than `enforcing_now`, and the
                // difference matters: a host whose ring has gone empty is not
                // enforcing, but it is exactly the host that most needs to
                // look at the store again — the reload is how it recovers.
                let reload = self.reload_if_due(now, self.enforce_requested);
                if reload != Reload::Done {
                    return Err(self.report_missing(&kid, request_id, reload.as_str()));
                }
                match check() {
                    Ok(v) => {
                        info!(
                            kid,
                            request_id, "keyring reload resolved a previously unknown key"
                        );
                        Ok(v)
                    }
                    Err(kid) => Err(self.report_missing(&kid, request_id, "reloaded")),
                }
            }
        }
    }

    /// Verify against the current ring. `Err(kid)` means **only**
    /// [`VerifyError::UnknownKid`]; every other error is already a final
    /// answer and comes back as its `Outcome`.
    fn check(
        &self,
        body: &[u8],
        headers: &SigHeaders,
        request_id: &str,
        now_ms: i64,
    ) -> Result<Outcome, String> {
        let ring = lock(&self.ring);
        match verify(&ring, body, headers, now_ms) {
            Ok(v) => {
                if v.policy.audit_every_use {
                    // A break-glass key whose use nobody investigates is a
                    // second production key, so this is unconditional and
                    // deliberately not rate-limited.
                    warn!(kid = v.kid, request_id, "command signed by an audited key");
                }
                Ok(Outcome::Verified)
            }
            Err(VerifyError::Unsigned) => Ok(Outcome::Unsigned),
            Err(VerifyError::UnknownKid { kid }) => Err(kid),
            Err(e @ VerifyError::Stale { .. }) => {
                warn!(error = %e, request_id, "command signature is past its freshness bound");
                Ok(Outcome::Stale)
            }
            Err(e) => {
                warn!(error = %e, request_id, "command signature did not verify");
                Ok(Outcome::Invalid)
            }
        }
    }

    /// Decide what to do with one received command payload, whichever protocol
    /// it uses. The single entry point both receive paths call, so the live
    /// subscription and the replay consumer cannot disagree.
    ///
    /// A payload is an envelope if and only if it is a JSON object with a
    /// `kind` key. No `kind` is the legacy form and takes exactly the old
    /// path (parse, observe, refuse only on an enforcing host). A `kind` that
    /// is null, not a string, or not [`ENVELOPE_KIND_V2`] is refused rather
    /// than falling back to the legacy parse — otherwise a new-form payload
    /// could be smuggled in under the old rules.
    ///
    /// An envelope is verified whether or not this host enforces: it is new
    /// traffic, so there is nothing to stay compatible with.
    ///
    /// Pure of side effects beyond the existing signature-outcome report and
    /// logging: no result is published for a refused envelope, because the
    /// host may never have been its recipient, and the request id is not
    /// consumed.
    pub fn admit(&self, payload: &[u8], headers: &SigHeaders, subject: &str) -> Admission {
        self.admit_at(
            payload,
            headers,
            subject,
            chrono::Utc::now(),
            Instant::now(),
        )
    }

    fn admit_at(
        &self,
        payload: &[u8],
        headers: &SigHeaders,
        subject: &str,
        now: chrono::DateTime<chrono::Utc>,
        instant: Instant,
    ) -> Admission {
        if has_kind(payload) {
            return self.admit_envelope(payload, headers, subject, now, instant);
        }
        let cmd: Command = match serde_json::from_slice(payload) {
            Ok(c) => c,
            Err(e) => return Admission::Undecodable(e),
        };
        let outcome = self.observe_at_instant(
            payload,
            headers,
            &cmd.request_id,
            now.timestamp_millis(),
            instant,
        );
        match self.refusal(outcome) {
            Some(reason) => Admission::RefusedLegacy { cmd, reason },
            None => Admission::Run {
                cmd,
                envelope_deadline: None,
            },
        }
    }

    fn admit_envelope(
        &self,
        payload: &[u8],
        headers: &SigHeaders,
        subject: &str,
        now: chrono::DateTime<chrono::Utc>,
        instant: Instant,
    ) -> Admission {
        // Before any signature work, and deliberately not reported as a
        // signing-state transition: an unrecognised kind is reachable by
        // anyone who can put bytes on the subject, so letting it move the
        // reported state would hand them the fleet's event stream.
        if kind_of(payload).as_deref() != Some(ENVELOPE_KIND_V2) {
            warn!(
                subject,
                kind = ?kind_of(payload),
                "REFUSED: payload has an unrecognised command kind"
            );
            return Admission::Refused;
        }
        let result = self.with_reload("", instant, || {
            let ring = lock(&self.ring);
            match verify_envelope(&ring, payload, headers, &self.pc_id, now) {
                Err(EnvelopeError::Signature(VerifyError::UnknownKid { kid })) => Err(kid),
                other => Ok(other),
            }
        });
        let (outcome, verified) = match result {
            Err(missing) => (missing, None),
            Ok(Ok(v)) => {
                if lock(&self.ring)
                    .get(&v.kid)
                    .is_some_and(|(_, _, p)| p.audit_every_use)
                {
                    warn!(
                        kid = %v.kid,
                        request_id = %v.command.request_id,
                        "command signed by an audited key"
                    );
                }
                (Outcome::Verified, Some(v))
            }
            Ok(Err(e)) => {
                warn!(subject, error = %e, "REFUSED: command envelope did not verify");
                (envelope_outcome(&e), None)
            }
        };
        self.report_transition(outcome);
        match verified {
            Some(v) => Admission::Run {
                envelope_deadline: Some(v.start_deadline),
                cmd: v.command,
            },
            None => Admission::Refused,
        }
    }

    /// Classify and log a key we still do not hold after doing what we can.
    fn report_missing(&self, kid: &str, request_id: &str, reload: &str) -> Outcome {
        let ring = lock(&self.ring);
        if ring.is_empty() {
            warn!(
                kid,
                request_id,
                reload,
                concat!(
                    "command is signed but this agent holds no keys — provision ",
                    keyring_location!()
                )
            );
            Outcome::Unprovisioned
        } else {
            warn!(
                kid,
                request_id,
                reload,
                known = ?ring.kids().collect::<Vec<_>>(),
                "command signed by a key this agent does not have"
            );
            Outcome::UnknownKid
        }
    }

    /// Pull the ring from the store if the rate limit allows, reporting which
    /// of the three things happened — the distinction reaches the log line an
    /// operator reads when a machine will not verify, and "we did not look" and
    /// "we looked and the value is broken" send them to different places.
    fn reload_if_due(&self, now: Instant, force: bool) -> Reload {
        let mut last = lock(&self.last_reload);
        // `checked_duration_since` rather than subtraction: an `Instant` from
        // before the recorded one would panic on the underflow, and a test (or
        // a future caller) passing a non-monotonic clock should not take the
        // agent down.
        let floor = if force {
            RELOAD_FORCED_MIN_INTERVAL
        } else {
            RELOAD_MIN_INTERVAL
        };
        if now.checked_duration_since(*last).unwrap_or_default() < floor {
            return Reload::RateLimited;
        }
        // Consumed even when the load fails: a corrupt value plus a stream of
        // unknown-key commands would otherwise be one read per command, which
        // is the case the floor exists for.
        *last = now;
        // `last_reload` is released here; `pull` takes `last_raw` itself, and
        // that is what serialises concurrent readers — this floor only bounds
        // how often the command path *asks*.
        drop(last);
        self.pull()
    }

    /// Re-read the store and adopt it if it changed.
    ///
    /// The `last_raw` guard is taken **before** the read, not after, and that
    /// ordering is load-bearing now that two callers reach here independently
    /// (a command path via [`Verifier::reload_if_due`], and the heartbeat via
    /// [`Verifier::refresh_and_report`]). Reading first and locking second lets
    /// two concurrent pulls observe different values and commit in the wrong
    /// order — the one that read the *older* value taking the lock last and
    /// installing a stale ring. For a revocation that means the revoked key
    /// comes back.
    ///
    /// What the guard buys is ordering, and only ordering. It does **not**
    /// collapse two arrivals into one store read — the loader runs
    /// unconditionally inside the critical section, so serialised callers each
    /// read (`an_unchanged_store_is_not_reparsed` asserts exactly that). The
    /// saving on an unchanged store is the parse, not the read.
    fn pull(&self) -> Reload {
        let mut last_raw = lock(&self.last_raw);
        let raw = match (self.loader)() {
            Ok(raw) => raw,
            // Keep what we have. A refresh can only ever improve the ring or
            // leave it alone — never destroy a working one — which is what
            // makes a skipped or failed one a delay rather than an outage.
            Err(e) => {
                warn!(error = %e, "keyring refresh failed — keeping the keys already loaded");
                return Reload::Failed;
            }
        };
        if *last_raw == raw {
            // The store has not changed, so neither has the ring. Returning
            // before the parse is what makes a per-heartbeat refresh cost a
            // single memory-mapped read: the expensive part is
            // `VerifyingKey::from_bytes`, which decompresses an Ed25519 point
            // per entry.
            return Reload::Unchanged;
        }
        match parse_raw(raw.as_deref()) {
            Ok(fresh) => {
                info!(
                    kids = ?fresh.kids().collect::<Vec<_>>(),
                    "command keyring changed — adopted"
                );
                *lock(&self.ring) = fresh;
                *last_raw = raw;
                Reload::Done
            }
            Err(e) => {
                // Do NOT record the raw value: leaving `last_raw` alone means
                // the next refresh tries again rather than treating a
                // half-written value as the new normal and never re-reading it.
                warn!(error = %e, "keyring changed but is unreadable — keeping the keys already loaded");
                Reload::Failed
            }
        }
    }

    /// Re-read the store, ignoring the command-path rate limit, and report both
    /// the keys now in force (as `kid:fingerprint`) and whether this host is
    /// enforcing. Called once per heartbeat.
    ///
    /// The two are returned together, under one lock, because they are only
    /// meaningful as a pair: "holds the right ring but is not enforcing" and
    /// "is enforcing" are the two halves of the stage-3 work queue, and a
    /// reload landing between two separate reads would let a heartbeat describe
    /// a machine that never existed — a ring with keys alongside the
    /// `enforcing: false` that an *empty* ring produces.
    ///
    /// This is what makes **revocation** work. The command-path reload fires
    /// only on [`VerifyError::UnknownKid`], which covers a ring that is
    /// missing a key — but a *revoked* key is one the ring still has, so every
    /// command signed with it verifies, no unknown-key outcome ever occurs, and
    /// nothing would trigger a re-read. Removing a key from the store would
    /// then have no effect until the agent restarted.
    ///
    /// It also keeps the reported ring honest. `command_keys` reports what this
    /// agent would accept *now*, and a newly added key is invisible to the
    /// command path until something signs with it — so a fleet-wide "has the
    /// new key landed everywhere" check would answer no forever.
    ///
    /// **The heartbeat interval therefore bounds revocation latency.** That
    /// coupling is deliberate but worth knowing: an operator who widens
    /// `heartbeat_interval` for bandwidth also widens the window in which a
    /// revoked key keeps working.
    pub fn refresh_and_report(&self) -> (Vec<String>, bool) {
        self.pull();
        let ring = lock(&self.ring);
        (
            ring.kid_fingerprints().collect(),
            self.enforcing_with(&ring),
        )
    }

    /// Emit an obs event when the class changes, so the fleet view shows which
    /// machines are verifying, which are still unsigned, and which are stuck
    /// on a key they never received.
    ///
    /// That last state is the reason this reporting exists at all: an agent
    /// missing a key looks, from the operator's side, exactly like a backend
    /// that stopped sending commands. Without a signal reaching the fleet
    /// view, a botched rotation is invisible until someone notices work is not
    /// running — the same class of failure as the #1145 throttle that passed
    /// two static reviews and did nothing on real hardware.
    fn report_transition(&self, outcome: Outcome) {
        let mut last = lock(&self.last);
        let Some(transition) = step(*last, outcome) else {
            return;
        };

        let event = build_event(&self.pc_id, &transition, chrono::Utc::now());
        // Commit `last` only after the event is safely queued. Marking it
        // first would mean a failed enqueue loses the transition permanently:
        // the next command carries the same outcome, `step` sees no change,
        // and the report never happens. Leaving `last` alone instead makes the
        // next command retry — which matters most for exactly the state this
        // reporting exists for, since an agent stuck on a key it never
        // received keeps producing that outcome.
        match crate::obs_outbox::enqueue(&self.obs_dir, &event) {
            Ok(_path) => *last = Some(outcome),
            Err(e) => warn!(
                error = %e,
                kind = outcome.kind(),
                "command_verify: enqueue failed — will retry on the next command"
            ),
        }
    }
}

/// What the receive path should do with a payload.
#[derive(Debug)]
pub enum Admission {
    /// Run it. `envelope_deadline` is the latest moment it may **start**, set
    /// only for a verified v2 envelope; legacy commands carry none.
    Run {
        cmd: Command,
        envelope_deadline: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// A legacy command refused by a host that enforces. The caller publishes
    /// the signature-refusal result with `reason`, as it always has.
    RefusedLegacy { cmd: Command, reason: &'static str },
    /// An envelope (or a payload with an unrecognised `kind`) that was
    /// refused. Already logged and reported; nothing is published, since the
    /// host may never have been its recipient.
    Refused,
    /// A legacy payload that is not a `Command` at all.
    Undecodable(serde_json::Error),
}

/// Whether the payload is a JSON object carrying a `kind` key, whatever its
/// value. Malformed JSON is not an envelope: it falls to the legacy parse and
/// fails there exactly as it always did.
fn has_kind(payload: &[u8]) -> bool {
    matches!(
        serde_json::from_slice::<serde_json::Value>(payload),
        Ok(serde_json::Value::Object(o)) if o.contains_key("kind")
    )
}

/// The `kind` as a string, `None` when absent, null or not a string.
fn kind_of(payload: &[u8]) -> Option<String> {
    match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(serde_json::Value::Object(o)) => o.get("kind")?.as_str().map(str::to_owned),
        _ => None,
    }
}

/// Map an envelope refusal onto the coarse class that gets reported.
fn envelope_outcome(e: &EnvelopeError) -> Outcome {
    match e {
        EnvelopeError::Signature(VerifyError::Unsigned) => Outcome::Unsigned,
        EnvelopeError::Signature(VerifyError::Stale { .. }) => Outcome::Stale,
        EnvelopeError::Signature(_) => Outcome::Invalid,
        EnvelopeError::ClockAhead { .. } => Outcome::ClockAhead,
        EnvelopeError::Expired { .. } | EnvelopeError::KeyAgeExceeded { .. } => Outcome::Expired,
        EnvelopeError::Misaddressed { .. } => Outcome::Misaddressed,
        EnvelopeError::UnknownKind(_)
        | EnvelopeError::Malformed(_)
        | EnvelopeError::MissingRecipient
        | EnvelopeError::MissingExpiry
        | EnvelopeError::MalformedExpiry(_)
        | EnvelopeError::Reversed
        | EnvelopeError::OverCeiling { .. } => Outcome::EnvelopeRejected,
    }
}

/// One reported change of signing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Transition {
    from: Option<Outcome>,
    to: Outcome,
}

/// Decide whether an outcome is worth reporting.
///
/// Pure, and separate from the enqueue, so the rule this PR rests on — report
/// on change, not per command — is testable without a filesystem. Same shape
/// as `idle_sampler::step`, which extracts its transition rule for the same
/// reason.
fn step(last: Option<Outcome>, outcome: Outcome) -> Option<Transition> {
    if last == Some(outcome) {
        return None;
    }
    Some(Transition {
        from: last,
        to: outcome,
    })
}

fn build_event(pc_id: &str, t: &Transition, at: chrono::DateTime<chrono::Utc>) -> ObsEvent {
    ObsEvent {
        pc_id: pc_id.to_string(),
        at,
        kind: t.to.kind().to_string(),
        source: SOURCE.to_string(),
        // Stable per-transition key so an outbox redelivery dedups against the
        // backend's UNIQUE(pc_id, source, event_record_id).
        event_record_id: Some(format!("{}:{}", t.to.kind(), at.timestamp_millis())),
        payload: serde_json::json!({
            "from": t.from.map(|p| p.kind()),
            "to": t.to.kind(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use kanade_shared::signing::sign;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn test_dir() -> std::path::PathBuf {
        std::env::temp_dir().join("kanade-command-verify-test")
    }

    /// Render a ring back to the registry value it would have been stored as.
    ///
    /// The loader now yields raw text so an unchanged store can skip parsing,
    /// so the fixtures have to round-trip through that same text — otherwise
    /// they would exercise a path production never takes.
    fn ring_to_json(ring: &KeyRing) -> Option<String> {
        let kids: Vec<&str> = ring.kids().collect();
        if kids.is_empty() {
            // An empty ring is the *absent* value, not `[]`: that is what
            // `read_keyring_raw` returns when the registry has no such value.
            return None;
        }
        let entries: Vec<serde_json::Value> = kids
            .iter()
            .map(|kid| {
                let (_, vk, policy) = ring.get(kid).expect("kid came from this ring");
                match policy.max_age {
                    Some(d) => serde_json::json!({
                        "kid": kid,
                        "public_key": kanade_shared::signing::encode_public(vk),
                        "max_age_secs": d.as_secs(),
                    }),
                    None => serde_json::json!({
                        "kid": kid,
                        "public_key": kanade_shared::signing::encode_public(vk),
                    }),
                }
            })
            .collect();
        Some(serde_json::to_string(&entries).expect("serialising a ring is infallible"))
    }

    /// A verifier whose store never changes — the pre-refresh behaviour.
    fn verifier_with(ring: KeyRing) -> Verifier {
        let raw = ring_to_json(&ring);
        Verifier::with_loader("PC1".into(), test_dir(), Box::new(move || Ok(raw.clone())))
    }

    fn enforcing_with(ring: KeyRing) -> Verifier {
        let raw = ring_to_json(&ring);
        Verifier::with_loader_and_policy(
            "PC1".into(),
            test_dir(),
            Box::new(move || Ok(raw.clone())),
            true,
        )
    }

    fn backend_ring(kid: &str, sk: &SigningKey) -> KeyRing {
        let mut r = KeyRing::new();
        r.insert(kid, sk.verifying_key(), KeyPolicy::backend("backend"));
        r
    }

    /// What the heartbeat should report for this key. Computed rather than
    /// hard-coded so a test asserts "the fingerprint of THIS key", which is the
    /// property under test — a literal would still pass if the wrong key's
    /// fingerprint were reported.
    fn reported(kid: &str, sk: &SigningKey) -> String {
        format!(
            "{kid}:{}",
            kanade_shared::signing::fingerprint(&sk.verifying_key())
        )
    }

    /// A ring that is empty until `provision()` is called, counting how many
    /// times it was read — the shape of a machine whose key arrives while the
    /// agent is already running.
    /// What the fake store holds: the value a read would return — including a
    /// failure, which is a distinct case from an absent value — and how many
    /// reads have happened, which is what the dedup tests assert on.
    type StoreState = (Result<Option<String>, String>, usize);

    #[derive(Clone)]
    struct Store {
        inner: std::sync::Arc<Mutex<StoreState>>,
    }

    impl Default for Store {
        fn default() -> Self {
            Self {
                inner: std::sync::Arc::new(Mutex::new((Ok(None), 0))),
            }
        }
    }

    impl Store {
        fn provision(&self, ring: KeyRing) {
            lock(&self.inner).0 = Ok(ring_to_json(&ring));
        }
        /// What a partially-written `CommandKeys` value looks like from here:
        /// present, changed, and unparseable.
        fn corrupt(&self) {
            lock(&self.inner).0 = Ok(Some("[{\"kid\":\"half-writ".to_string()));
        }
        /// The store becoming unreadable, as distinct from holding rubbish.
        fn unreadable(&self) {
            lock(&self.inner).0 = Err("registry unavailable".into());
        }
        fn reads(&self) -> usize {
            lock(&self.inner).1
        }
        fn loader(&self) -> Loader {
            let inner = self.inner.clone();
            Box::new(move || {
                let mut g = lock(&inner);
                g.1 += 1;
                g.0.clone()
            })
        }
    }

    #[test]
    fn keyring_parses_the_provisioned_shape() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let raw = format!(
            r#"[{{"kid":"backend-1","public_key":"{}","label":"backend"}}]"#,
            b64(sk.verifying_key().as_bytes())
        );
        let ring = parse_keyring(&raw).expect("parses");
        let (kid, _, policy) = ring.get("backend-1").expect("present");
        assert_eq!(kid, "backend-1");
        assert_eq!(policy.label, "backend");
        assert_eq!(policy.max_age, None);
        assert!(!policy.audit_every_use);
    }

    #[test]
    fn a_max_age_entry_becomes_a_break_glass_policy() {
        // The shape a break-glass key is provisioned in: bounded lifetime and
        // audited on every use, both carried by the key rather than by a call
        // site that could forget.
        let sk = SigningKey::from_bytes(&[8u8; 32]);
        let raw = format!(
            r#"[{{"kid":"bg","public_key":"{}","max_age_secs":300}}]"#,
            b64(sk.verifying_key().as_bytes())
        );
        let ring = parse_keyring(&raw).unwrap();
        let (_, _, policy) = ring.get("bg").unwrap();
        assert_eq!(policy.max_age, Some(std::time::Duration::from_secs(300)));
        assert!(policy.audit_every_use);
        // Label defaults to the kid so a log line is never blank.
        assert_eq!(policy.label, "bg");
    }

    #[test]
    fn a_malformed_keyring_is_rejected_rather_than_half_loaded() {
        // Half a keyring is worse than none: at stage 3 it would silently
        // reject commands from the key that failed to parse while accepting
        // the others, which reads as "some machines stopped working".
        let good = b64(SigningKey::from_bytes(&[1u8; 32])
            .verifying_key()
            .as_bytes());
        let raw = format!(
            r#"[{{"kid":"ok","public_key":"{good}"}},{{"kid":"bad","public_key":"not base64"}}]"#
        );
        assert!(parse_keyring(&raw).is_err());

        // Right length check: a 31-byte key must not be padded into place.
        let short = b64(&[0u8; 31]);
        assert!(parse_keyring(&format!(r#"[{{"kid":"s","public_key":"{short}"}}]"#)).is_err());

        // Not JSON at all.
        assert!(parse_keyring("{{{").is_err());
    }

    #[test]
    fn a_duplicate_kid_is_refused_rather_than_silently_collapsed() {
        // The failure this prevents is invisible: `KeyRing` is keyed by id, so
        // two entries sharing one would leave whichever came last in the map and
        // every command signed by the other would fail verification with no
        // signal pointing at the cause. Reachable in practice — the array is
        // hand-assembled, and two break-glass keys minted close together used
        // to default to the same id.
        let a = b64(SigningKey::from_bytes(&[1u8; 32])
            .verifying_key()
            .as_bytes());
        let b = b64(SigningKey::from_bytes(&[2u8; 32])
            .verifying_key()
            .as_bytes());
        let raw =
            format!(r#"[{{"kid":"bg","public_key":"{a}"}},{{"kid":"bg","public_key":"{b}"}}]"#);
        let err = parse_keyring(&raw).unwrap_err();
        assert!(err.contains("bg"), "the error must name the id: {err}");
        assert!(err.contains("twice"), "{err}");

        // Distinct ids in the same array are the normal multi-signer case and
        // must keep working — this guard must not be a rotation blocker.
        let raw = format!(
            r#"[{{"kid":"backend-1","public_key":"{a}"}},{{"kid":"bg","public_key":"{b}","max_age_secs":900}}]"#
        );
        let ring = parse_keyring(&raw).expect("two distinct ids are fine");
        assert!(ring.get("backend-1").is_some());
        assert!(ring.get("bg").is_some());
    }

    #[test]
    fn an_empty_ring_reports_unsigned_and_unknown_but_never_verifies() {
        let ring = parse_keyring("[]").unwrap();
        assert!(ring.is_empty());
        let sk = SigningKey::from_bytes(&[3u8; 32]);

        // Unsigned traffic on an agent with no keys: normal until enforced.
        assert_eq!(
            verify(&ring, b"body", &SigHeaders::default(), 0),
            Err(VerifyError::Unsigned)
        );
        // Signed traffic it cannot check: reported, not silently accepted as
        // if it were unsigned.
        let headers = sign(&sk, "backend-1", b"body", 0);
        assert!(matches!(
            verify(&ring, b"body", &headers, 0),
            Err(VerifyError::UnknownKid { .. })
        ));
    }

    #[test]
    fn outcome_kinds_are_distinct_and_stable() {
        // These strings reach the SPA's Events filter and the backend's
        // UNIQUE key, so a collision or a rename is a data change, not a
        // cosmetic one.
        let kinds: Vec<_> = Outcome::ALL.iter().map(|o| o.kind()).collect();
        let unique: std::collections::BTreeSet<_> = kinds.iter().collect();
        assert_eq!(unique.len(), kinds.len(), "kinds must not collide");
        assert!(kinds.iter().all(|k| k.starts_with("command_signature")));
    }

    #[test]
    fn step_reports_the_baseline_then_only_on_change() {
        // First observation is always worth reporting: "this machine is
        // running unsigned commands" is a fact the fleet view needs even
        // though nothing changed to produce it.
        assert_eq!(
            step(None, Outcome::Unsigned),
            Some(Transition {
                from: None,
                to: Outcome::Unsigned
            })
        );
        // Steady state is silent — otherwise stages 1-2 emit thousands of
        // identical rows a day and bury the one that matters.
        assert_eq!(step(Some(Outcome::Unsigned), Outcome::Unsigned), None);
        // The transition that means the backend started signing.
        assert_eq!(
            step(Some(Outcome::Unsigned), Outcome::Verified),
            Some(Transition {
                from: Some(Outcome::Unsigned),
                to: Outcome::Verified
            })
        );
        // And the one that means a rotation went wrong.
        assert_eq!(
            step(Some(Outcome::Verified), Outcome::UnknownKid),
            Some(Transition {
                from: Some(Outcome::Verified),
                to: Outcome::UnknownKid
            })
        );
    }

    #[test]
    fn a_flapping_state_reports_each_way() {
        // Verified -> UnknownKid -> Verified is a keyring that was briefly
        // wrong. Both edges must appear, or the timeline shows a machine
        // entering a bad state and never leaving it.
        let mut last = None;
        let mut reported = Vec::new();
        for o in [
            Outcome::Verified,
            Outcome::Verified,
            Outcome::UnknownKid,
            Outcome::Verified,
        ] {
            if let Some(t) = step(last, o) {
                reported.push(t.to);
                last = Some(o);
            }
        }
        assert_eq!(
            reported,
            vec![Outcome::Verified, Outcome::UnknownKid, Outcome::Verified]
        );
    }

    #[test]
    fn a_failed_enqueue_leaves_the_transition_unreported_so_it_retries() {
        // The ordering that matters: `last` is only advanced once the event is
        // queued. Simulating the failure path here, since committing first
        // would drop the transition permanently — the next command carries the
        // same outcome, `step` sees no change, and the state this reporting
        // exists for is never surfaced.
        let mut last: Option<Outcome> = None;
        let enqueue_ok = false;

        if let Some(_t) = step(last, Outcome::UnknownKid)
            && enqueue_ok
        {
            last = Some(Outcome::UnknownKid);
        }
        assert_eq!(last, None, "a failed enqueue must not mark it reported");

        // Next command, same outcome: still reportable.
        assert!(
            step(last, Outcome::UnknownKid).is_some(),
            "the retry has to be possible"
        );
    }

    #[test]
    fn the_event_carries_both_ends_of_the_transition() {
        // `from` is what makes the timeline readable: "was verifying, now on
        // an unknown key" reads differently from "has always been unknown".
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let e = build_event(
            "PC1",
            &Transition {
                from: Some(Outcome::Verified),
                to: Outcome::UnknownKid,
            },
            at,
        );
        assert_eq!(e.pc_id, "PC1");
        assert_eq!(e.source, SOURCE);
        assert_eq!(e.kind, "command_signature_unknown_key");
        assert_eq!(e.payload["from"], "command_signature_ok");
        assert_eq!(e.payload["to"], "command_signature_unknown_key");
        // Dedup key pins kind + instant so an outbox redelivery collapses
        // against the backend's UNIQUE constraint rather than duplicating.
        assert_eq!(
            e.event_record_id.as_deref(),
            Some("command_signature_unknown_key:1700000000000")
        );
    }

    #[test]
    fn the_first_event_reports_no_previous_state() {
        let at = chrono::DateTime::from_timestamp(0, 0).unwrap();
        let e = build_event(
            "PC1",
            &Transition {
                from: None,
                to: Outcome::Unsigned,
            },
            at,
        );
        assert!(e.payload["from"].is_null());
    }

    #[test]
    fn a_replayed_break_glass_command_is_reported_stale_not_invalid() {
        // The first-boot case the freshness bound exists for: the dedup cache
        // is empty, so nothing else would stop a week-old emergency command.
        // It must read as `Stale` rather than `Invalid` — the message is
        // genuine, and "someone forged this" would send an operator looking
        // for an intruder that isn't there.
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let mut ring = KeyRing::new();
        ring.insert(
            "break-glass",
            sk.verifying_key(),
            KeyPolicy::break_glass("break-glass", std::time::Duration::from_secs(300)),
        );
        let v = verifier_with(ring);

        let now = 1_700_000_000_000i64;
        let body = b"emergency";
        let week = 7 * 24 * 60 * 60 * 1000;

        assert_eq!(
            v.observe_at(body, &sign(&sk, "break-glass", body, now - week), "r1", now),
            Outcome::Stale
        );
        // The same key inside its window is fine.
        assert_eq!(
            v.observe_at(
                body,
                &sign(&sk, "break-glass", body, now - 1_000),
                "r2",
                now
            ),
            Outcome::Verified
        );
    }

    #[test]
    fn a_key_provisioned_after_boot_takes_effect_without_a_restart() {
        // Measured on the dev host: the key was distributed successfully at
        // 17:27 and the agent, running since 15:47, still reported an unknown
        // key hours later because the ring was read once at startup and never
        // again. This is that scenario as a test.
        let sk = SigningKey::from_bytes(&[11u8; 32]);
        let store = Store::default();
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());

        let now = 1_700_000_000_000i64;
        let body = b"job";
        let headers = sign(&sk, "backend-1", body, now);
        let boot = Instant::now();

        // Before provisioning: signed, and we hold nothing.
        assert_eq!(
            v.classify(body, &headers, "r1", now, boot),
            Outcome::Unprovisioned
        );

        store.provision(backend_ring("backend-1", &sk));

        // Still inside the rate-limit window — the ring on disk is right, but
        // we have not looked. Reporting the stale answer here is correct; what
        // must not happen is reporting it forever.
        assert_eq!(
            v.classify(body, &headers, "r2", now, boot),
            Outcome::Unprovisioned
        );

        // Past the window: the next unknown key pulls the ring and the same
        // command verifies. No restart, no redeploy.
        let later = boot + RELOAD_MIN_INTERVAL;
        assert_eq!(
            v.classify(body, &headers, "r3", now, later),
            Outcome::Verified
        );
    }

    #[test]
    fn enforcement_refuses_everything_that_is_not_verified() {
        let sk = SigningKey::from_bytes(&[31u8; 32]);
        let v = enforcing_with(backend_ring("backend-1", &sk));

        // The one that runs.
        assert!(v.refusal(Outcome::Verified).is_none());
        // Everything else does not — `Unsigned` above all, since refusing it
        // is what stage 3 is for.
        for o in [
            Outcome::Unsigned,
            Outcome::Unprovisioned,
            Outcome::UnknownKid,
            Outcome::Invalid,
            Outcome::Stale,
        ] {
            let reason = v.refusal(o).unwrap_or_else(|| panic!("{o:?} must refuse"));
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn an_enforcing_host_reloads_before_refusing_even_inside_the_rate_limit() {
        // The rejection bug the module doc warned about: a key provisioned
        // seconds ago, inside the reload window, would be refused for the rest
        // of it. Free to skip a reload while nothing is enforced; not free
        // once the answer is "refuse".
        let sk = SigningKey::from_bytes(&[41u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader_and_policy(
            "PC1".into(),
            test_dir(),
            store.loader(),
            true, // enforcing
        );
        let now = 1_700_000_000_000i64;
        let boot = Instant::now();

        // A key this host does not hold yet.
        let headers = sign(&sk, "backend-2", b"job", now);
        assert_eq!(
            v.classify(b"job", &headers, "r1", now, boot),
            Outcome::UnknownKid
        );

        // It lands on disk — well inside RELOAD_MIN_INTERVAL of the last read.
        let mut rotated = backend_ring("backend-1", &sk);
        rotated.insert(
            "backend-2",
            sk.verifying_key(),
            KeyPolicy::backend("backend (new)"),
        );
        store.provision(rotated);

        // A second later — deep inside RELOAD_MIN_INTERVAL, so a non-enforcing
        // host would still be rate-limited and would refuse. This one reads
        // again and accepts.
        //
        // A second, not zero: the forced path shortens the floor rather than
        // removing it, because removing it made every unknown-kid command do a
        // registry read while holding `last_reload`, serializing the command
        // paths behind attacker-chosen ids.
        assert_eq!(
            v.classify(
                b"job",
                &headers,
                "r2",
                now,
                boot + RELOAD_FORCED_MIN_INTERVAL
            ),
            Outcome::Verified,
            "an enforcing host must not refuse a key it already has on disk"
        );
    }

    #[test]
    fn the_forced_path_shortens_the_floor_rather_than_removing_it() {
        // Without any floor, every unknown-kid command on an enforcing host
        // does a registry read while holding `last_reload` — so concurrent
        // command paths serialize behind one I/O each, for traffic whose kid
        // an attacker picks. The cost that matters is the serialization, not
        // the microseconds.
        let sk = SigningKey::from_bytes(&[43u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader_and_policy("PC1".into(), test_dir(), store.loader(), true);
        let now = 1_700_000_000_000i64;
        let headers = sign(&sk, "backend-2", b"x", now);
        let base = Instant::now() + RELOAD_FORCED_MIN_INTERVAL;
        let reads = store.reads();

        // A burst inside one forced interval collapses to a single read.
        for i in 0..10 {
            assert_eq!(
                v.classify(
                    b"x",
                    &headers,
                    "r",
                    now,
                    base + RELOAD_FORCED_MIN_INTERVAL / 20 * i
                ),
                Outcome::UnknownKid
            );
        }
        assert_eq!(
            store.reads(),
            reads + 1,
            "a burst must not be one read each"
        );

        // And the next interval reads again — a floor, not a latch.
        assert_eq!(
            v.classify(
                b"x",
                &headers,
                "r",
                now,
                base + RELOAD_FORCED_MIN_INTERVAL * 2
            ),
            Outcome::UnknownKid
        );
        assert_eq!(store.reads(), reads + 2);
    }

    #[test]
    fn a_non_enforcing_host_still_respects_the_rate_limit() {
        // The bypass is scoped to enforcement. Without it, ordinary traffic
        // naming invented key ids would be one registry read per command.
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());
        let now = 1_700_000_000_000i64;
        let boot = Instant::now();
        let headers = sign(&sk, "backend-2", b"job", now);
        let reads = store.reads();

        for i in 0..5 {
            assert_eq!(
                v.classify(
                    b"job",
                    &headers,
                    "r",
                    now,
                    boot + RELOAD_MIN_INTERVAL / 20 * i
                ),
                Outcome::UnknownKid
            );
        }
        assert_eq!(store.reads(), reads, "still bounded when not enforcing");
    }

    #[test]
    fn a_host_that_is_not_enforcing_refuses_nothing() {
        // Stages 1-2, and the state every host is in until an operator flips
        // it. The outcome is still classified and reported; only the acting on
        // it is off.
        let sk = SigningKey::from_bytes(&[32u8; 32]);
        let v = verifier_with(backend_ring("backend-1", &sk));
        for o in Outcome::ALL {
            assert!(v.refusal(o).is_none(), "{o:?} must not refuse");
        }
    }

    #[test]
    fn a_ring_that_goes_empty_at_runtime_stops_enforcing_too() {
        // The constructor's empty-ring guard is not enough on its own: the
        // ring changes under a running agent. `read_keyring` maps an absent
        // registry value to `Ok(empty)` — a revoke an operator can intend, and
        // a window a provisioning script that deletes-then-writes passes
        // through — so a host that booted enforcing can reload into holding
        // nothing. Cached enforcement would then refuse every command,
        // including the one restoring its keys: the same bricking, reached by
        // the other door.
        let sk = SigningKey::from_bytes(&[51u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader_and_policy("PC1".into(), test_dir(), store.loader(), true);

        // Enforcing while it holds a key.
        assert!(v.refusal(Outcome::Unsigned).is_some());

        // Everything is revoked, and a command with an unknown kid drives the
        // reload that picks the empty ring up.
        store.provision(KeyRing::new());
        let t = Instant::now() + RELOAD_MIN_INTERVAL * 2;
        assert_eq!(
            v.classify(b"job", &sign(&sk, "backend-2", b"job", 0), "r1", 0, t),
            Outcome::Unprovisioned
        );

        assert!(
            v.refusal(Outcome::Unsigned).is_none(),
            "a host holding no keys must stop enforcing, not brick"
        );

        // And it resumes once keys come back — declining is a live response to
        // the ring, not a latch that needs a restart to clear.
        store.provision(backend_ring("backend-1", &sk));
        assert_eq!(
            v.classify(
                b"job",
                &sign(&sk, "backend-2", b"job", 0),
                "r2",
                0,
                t + RELOAD_MIN_INTERVAL * 2
            ),
            Outcome::UnknownKid
        );
        assert!(
            v.refusal(Outcome::Unsigned).is_some(),
            "enforcement must come back when the keys do"
        );
    }

    #[test]
    fn an_empty_ring_declines_to_enforce_rather_than_bricking_the_host() {
        // Asking a host with no keys to enforce is asking it to refuse every
        // command — including the one that would provision the keys, which is
        // the only remote way out. Declining loses nothing to an attacker
        // (emptying the ring needs local admin, and that already grants
        // arbitrary local execution) and turns a dead endpoint into a visible
        // misconfiguration.
        let v =
            Verifier::with_loader_and_policy("PC1".into(), test_dir(), Box::new(|| Ok(None)), true);
        assert!(
            v.refusal(Outcome::Unsigned).is_none(),
            "an empty ring must not enforce"
        );
    }

    #[test]
    fn every_refusal_reason_says_what_to_do_next() {
        // The reason becomes the refused command's stderr, and it is the only
        // thing the issuer sees — during an incident, with the obs event stuck
        // in an outbox behind a backend that is down. A message that only
        // restates the outcome would leave them where they started.
        for o in Outcome::ALL {
            if !o.is_refusal() {
                continue;
            }
            let r = o.refusal_reason();
            assert!(r.len() > 40, "{o:?} reason is too thin: {r}");
            assert!(
                r.contains("host") || r.contains("key") || r.contains("clock"),
                "{o:?} reason should point somewhere: {r}"
            );
        }
    }

    #[test]
    fn revoking_a_key_takes_effect_without_a_restart() {
        // The gap this whole change exists for. The command path re-reads only
        // on `UnknownKid` — "a key is missing" — and a revoked key is the
        // opposite shape: still present in memory, so every command signed
        // with it verifies, no unknown-key outcome ever fires, and nothing
        // asks the store again. Removing it from `CommandKeys` had no effect
        // until the agent restarted, which makes "the array is replaced, not
        // merged, so emergency revocation is possible" untrue in practice.
        let backend = SigningKey::from_bytes(&[61u8; 32]);
        let compromised = SigningKey::from_bytes(&[62u8; 32]);
        let mut both = backend_ring("backend-1", &backend);
        both.insert(
            "leaked",
            compromised.verifying_key(),
            KeyPolicy::backend("leaked"),
        );
        let store = Store::default();
        store.provision(both);
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());

        let now = 1_700_000_000_000i64;
        let t = Instant::now() + RELOAD_MIN_INTERVAL * 2;
        let signed = sign(&compromised, "leaked", b"payload", now);

        // Before: the leaked key verifies, and note that it does so WITHOUT
        // ever producing an unknown-key outcome — which is exactly why no
        // re-read was ever triggered.
        assert_eq!(
            v.classify(b"payload", &signed, "r1", now, t),
            Outcome::Verified
        );

        // The operator revokes it by writing a ring without it.
        store.provision(backend_ring("backend-1", &backend));

        // Command traffic alone still does not notice — the key is in memory.
        assert_eq!(
            v.classify(b"payload", &signed, "r2", now, t),
            Outcome::Verified,
            "this is the gap: the command path has no reason to re-read"
        );

        // The heartbeat refresh is what closes it.
        let (kids, _) = v.refresh_and_report();
        assert_eq!(kids, vec![reported("backend-1", &backend)]);
        assert_eq!(
            v.classify(b"payload", &signed, "r3", now, t),
            Outcome::UnknownKid,
            "a revoked key must stop verifying"
        );
    }

    #[test]
    fn two_rings_sharing_a_kid_but_not_a_key_report_differently() {
        // The state #1229 exists to make visible. Both hosts answer
        // "backend-1", both look correct in any kid-only view, and the one
        // holding the wrong bytes refuses every command once enforcement is on
        // — without self-healing, because the reload-on-unknown-key path never
        // fires for a key that is *present*.
        let right = SigningKey::from_bytes(&[70u8; 32]);
        let wrong = SigningKey::from_bytes(&[71u8; 32]);

        let good = Store::default();
        good.provision(backend_ring("backend-1", &right));
        let bad = Store::default();
        bad.provision(backend_ring("backend-1", &wrong));

        let a = Verifier::with_loader("PC1".into(), test_dir(), good.loader());
        let b = Verifier::with_loader("PC2".into(), test_dir(), bad.loader());

        let (a, b) = (a.trusted_keys(), b.trusted_keys());
        assert_ne!(a, b, "a fleet view must be able to tell these apart");
        assert!(
            a[0].starts_with("backend-1:") && b[0].starts_with("backend-1:"),
            "and the kid must still be greppable on its own: {a:?} {b:?}"
        );
    }

    #[test]
    fn a_fingerprint_survives_a_reload_that_changes_nothing_else() {
        // Reporting is downstream of the ring, not of the read: the value must
        // not depend on whether this heartbeat happened to re-parse.
        let sk = SigningKey::from_bytes(&[72u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());

        let (first, _) = v.refresh_and_report();
        assert_eq!(v.pull(), Reload::Unchanged);
        assert_eq!(v.refresh_and_report().0, first);
        assert_eq!(first, vec![reported("backend-1", &sk)]);
    }

    #[test]
    fn a_host_that_is_not_configured_to_enforce_reports_false() {
        // Every machine in the fleet today: a complete ring, and not enforcing.
        // The pair is the point — the ring alone cannot express it.
        let sk = SigningKey::from_bytes(&[73u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());

        let (keys, enforcing) = v.refresh_and_report();
        assert_eq!(keys, vec![reported("backend-1", &sk)]);
        assert!(!enforcing, "with_loader does not request enforcement");
    }

    #[test]
    fn an_enforcing_host_that_loses_its_ring_reports_false() {
        // The effective state, not the configured one. `RequireSignedCommands`
        // is still set here, but the agent declines to enforce on an empty ring
        // — refusing everything would include the command that restores the
        // keys — so a host in this state is NOT enforcing, and reporting the
        // registry value would describe a machine that does not exist.
        //
        // Fleet-wide this is the difference between "someone wiped a ring and
        // that host silently stopped enforcing" and a healthy host, which the
        // registry value cannot tell apart.
        let sk = SigningKey::from_bytes(&[74u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader_and_policy("PC1".into(), test_dir(), store.loader(), true);

        let (_, enforcing) = v.refresh_and_report();
        assert!(enforcing, "a requested, populated ring enforces");

        store.provision(KeyRing::new());
        let (keys, enforcing) = v.refresh_and_report();
        assert!(keys.is_empty());
        assert!(
            !enforcing,
            "an empty ring cannot enforce, whatever the registry says"
        );
    }

    #[test]
    fn reporting_does_not_fire_the_empty_ring_warning() {
        // The reporting path must stay side-effect free: the empty-ring log is
        // about declining to ACT, and firing it from an observation would make
        // its volume a function of the heartbeat interval. The fleet-visible
        // `enforcing: false` is the signal here — and unlike a log line, it can
        // be counted.
        let sk = SigningKey::from_bytes(&[75u8; 32]);
        let store = Store::default();
        store.provision(KeyRing::new());
        let v = Verifier::with_loader_and_policy("PC1".into(), test_dir(), store.loader(), true);

        for _ in 0..3 {
            assert!(!v.refresh_and_report().1);
        }
        assert!(
            !v.empty_ring_warned
                .load(std::sync::atomic::Ordering::Relaxed),
            "reporting must not consume the one-shot warning the command path owns"
        );

        // And the command path still owns it.
        let _ = v.refusal(Outcome::Unsigned);
        assert!(
            v.empty_ring_warned
                .load(std::sync::atomic::Ordering::Relaxed),
            "the decision path is what warns"
        );
        let _ = sk;
    }

    #[test]
    fn an_unchanged_store_is_not_reparsed() {
        // What makes a per-heartbeat refresh affordable. The registry read is
        // cheap; `VerifyingKey::from_bytes` is not — it decompresses an
        // Ed25519 point per entry. Comparing the raw value first skips that
        // entirely in the steady state, which is every heartbeat but the ones
        // that follow a real change.
        let sk = SigningKey::from_bytes(&[63u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());
        let reads = store.reads();

        assert_eq!(v.pull(), Reload::Unchanged);
        assert_eq!(v.pull(), Reload::Unchanged);
        // It still READ the store each time — that is the cheap part, and
        // skipping it would mean never noticing a change.
        assert_eq!(store.reads(), reads + 2);

        // A real change is adopted.
        store.provision(backend_ring("backend-2", &sk));
        assert_eq!(v.pull(), Reload::Done);
        assert_eq!(v.pull(), Reload::Unchanged);
    }

    #[test]
    fn a_half_written_value_is_retried_rather_than_remembered() {
        // `last_raw` is deliberately NOT updated on a parse failure. Recording
        // it would treat a half-written value as the new normal: the next
        // refresh would compare equal, skip, and never look again — so a
        // provisioning write caught mid-flight would strand the machine on its
        // old ring until a restart, silently.
        let sk = SigningKey::from_bytes(&[64u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());

        store.corrupt();
        assert_eq!(v.pull(), Reload::Failed);
        assert_eq!(v.pull(), Reload::Failed, "it must keep retrying");
        // And the working ring is still there.
        assert_eq!(v.trusted_keys(), vec![reported("backend-1", &sk)]);

        // Once the write completes, it is adopted.
        store.provision(backend_ring("backend-2", &sk));
        assert_eq!(v.pull(), Reload::Done);
        assert_eq!(v.trusted_keys(), vec![reported("backend-2", &sk)]);
    }

    #[test]
    fn an_unreadable_store_keeps_the_ring_and_does_not_poison_the_cache() {
        let sk = SigningKey::from_bytes(&[65u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());

        store.unreadable();
        assert_eq!(v.pull(), Reload::Failed);
        assert_eq!(v.trusted_keys(), vec![reported("backend-1", &sk)]);

        store.provision(backend_ring("backend-2", &sk));
        assert_eq!(v.pull(), Reload::Done);
    }

    #[test]
    fn a_reload_that_cannot_be_read_keeps_the_working_ring() {
        // The regression this split exists for. The provisioning job writes
        // `CommandKeys` while the agent is running, so a reload can catch a
        // partial write. Collapsing that into "empty" — which is right at boot,
        // where there is nothing to lose — would take a verifying machine down
        // to holding no keys, and at stage 3 that is a machine refusing every
        // command rather than one delayed command.
        let sk = SigningKey::from_bytes(&[21u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());

        let now = 1_700_000_000_000i64;
        let good = sign(&sk, "backend-1", b"job", now);
        let t = Instant::now() + RELOAD_MIN_INTERVAL * 2;
        assert_eq!(v.classify(b"job", &good, "r1", now, t), Outcome::Verified);

        // Someone is mid-write on the registry value.
        store.corrupt();

        // An unknown key drives a reload, which fails.
        let unknown = sign(&sk, "backend-2", b"job", now);
        assert_eq!(
            v.classify(b"job", &unknown, "r2", now, t + RELOAD_MIN_INTERVAL),
            Outcome::UnknownKid,
            "a failed reload must not turn this into Unprovisioned"
        );

        // And the key we already had still verifies.
        assert_eq!(
            v.classify(b"job", &good, "r3", now, t + RELOAD_MIN_INTERVAL * 2),
            Outcome::Verified,
            "the working ring must survive a failed reload"
        );
    }

    #[test]
    fn only_an_unknown_key_reaches_the_store() {
        // The reload trigger is attacker-reachable — anyone who can place bytes
        // on a command subject picks the `kid`. If unsigned or invalid traffic
        // also reloaded, every delivered command would become a registry read
        // and the rate limit would be the only thing standing between the fleet
        // and a remote I/O amplifier.
        let sk = SigningKey::from_bytes(&[12u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());
        let after_boot = Instant::now() + RELOAD_MIN_INTERVAL * 10;
        let now = 1_700_000_000_000i64;
        let reads = store.reads();

        // Unsigned: normal stage-1/2 traffic.
        assert_eq!(
            v.classify(b"x", &SigHeaders::default(), "r1", now, after_boot),
            Outcome::Unsigned
        );
        // A signature that does not match the bytes.
        let forged = sign(&SigningKey::from_bytes(&[99u8; 32]), "backend-1", b"x", now);
        assert_eq!(
            v.classify(b"x", &forged, "r2", now, after_boot),
            Outcome::Invalid
        );
        // Malformed.
        let partial = SigHeaders {
            sig_b64: Some("AAAA".into()),
            kid: None,
            alg: None,
            at_ms: None,
        };
        assert_eq!(
            v.classify(b"x", &partial, "r3", now, after_boot),
            Outcome::Invalid
        );
        assert_eq!(store.reads(), reads, "none of these may touch the store");

        // And the one that does.
        let unknown = sign(&sk, "backend-2", b"x", now);
        assert_eq!(
            v.classify(b"x", &unknown, "r4", now, after_boot),
            Outcome::UnknownKid
        );
        assert_eq!(store.reads(), reads + 1);
    }

    #[test]
    fn repeated_unknown_keys_reload_at_most_once_per_interval() {
        let sk = SigningKey::from_bytes(&[13u8; 32]);
        let store = Store::default();
        store.provision(backend_ring("backend-1", &sk));
        let v = Verifier::with_loader("PC1".into(), test_dir(), store.loader());
        let now = 1_700_000_000_000i64;
        let headers = sign(&sk, "backend-2", b"x", now);
        let base = Instant::now() + RELOAD_MIN_INTERVAL;
        let reads = store.reads();

        for i in 0..20 {
            // Twenty commands spread across less than one interval.
            let t = base + RELOAD_MIN_INTERVAL / 40 * i;
            assert_eq!(v.classify(b"x", &headers, "r", now, t), Outcome::UnknownKid);
        }
        assert_eq!(
            store.reads(),
            reads + 1,
            "a flood of invented key ids must not become a flood of store reads"
        );

        // The floor is per interval, not once ever — otherwise a key
        // provisioned after the first miss would never be picked up.
        assert_eq!(
            v.classify(b"x", &headers, "r", now, base + RELOAD_MIN_INTERVAL * 2),
            Outcome::UnknownKid
        );
        assert_eq!(store.reads(), reads + 2);
    }

    #[test]
    fn an_empty_ring_and_a_missing_key_are_different_states() {
        // They need opposite responses — "provisioning never reached this
        // machine" vs "this machine missed a rotation" — and during stages 1-2
        // every unprovisioned machine would otherwise raise the rotation alarm,
        // which is how an alarm gets ignored before it ever matters.
        let sk = SigningKey::from_bytes(&[14u8; 32]);
        let now = 1_700_000_000_000i64;
        let headers = sign(&sk, "backend-2", b"x", now);
        let t = Instant::now() + RELOAD_MIN_INTERVAL * 2;

        let empty = Verifier::with_loader("PC1".into(), test_dir(), Box::new(|| Ok(None)));
        assert_eq!(
            empty.classify(b"x", &headers, "r1", now, t),
            Outcome::Unprovisioned
        );

        let other = verifier_with(backend_ring("backend-1", &sk));
        assert_eq!(
            other.classify(b"x", &headers, "r2", now, t),
            Outcome::UnknownKid
        );
    }

    #[test]
    fn a_non_monotonic_clock_does_not_panic() {
        // `Instant` subtraction panics on underflow. The clock is injected, so
        // an out-of-order value is reachable; taking the agent down over it
        // would be a worse outcome than skipping one reload.
        let sk = SigningKey::from_bytes(&[15u8; 32]);
        let v = verifier_with(backend_ring("backend-1", &sk));
        let now = 1_700_000_000_000i64;
        let headers = sign(&sk, "backend-2", b"x", now);
        // `checked_sub` because `Instant - Duration` panics when the result
        // would precede the platform's monotonic epoch — which is exactly the
        // boot-adjacent case a CI runner can be in.
        let Some(past) = Instant::now().checked_sub(RELOAD_MIN_INTERVAL * 3) else {
            return;
        };
        assert_eq!(
            v.classify(b"x", &headers, "r1", now, past),
            Outcome::UnknownKid
        );
    }

    #[test]
    fn the_ordinary_signer_is_never_stale() {
        // JetStream replay hands back commands retained for 7 days; a bound on
        // the backend key would turn every reconnect into a rejection.
        let sk = SigningKey::from_bytes(&[4u8; 32]);
        let mut ring = KeyRing::new();
        ring.insert(
            "backend-1",
            sk.verifying_key(),
            KeyPolicy::backend("backend"),
        );
        let v = verifier_with(ring);

        let now = 1_700_000_000_000i64;
        let week = 7 * 24 * 60 * 60 * 1000;
        assert_eq!(
            v.observe_at(
                b"job",
                &sign(&sk, "backend-1", b"job", now - week),
                "r1",
                now
            ),
            Outcome::Verified
        );
    }

    // ---- file-based keyring (hosts with no registry) ----

    fn ring_json(kid: &str, sk: &SigningKey) -> String {
        ring_to_json(&backend_ring(kid, sk)).expect("non-empty ring")
    }

    /// The uid that owns files this test process creates — the stand-in for
    /// root, which a test cannot be.
    fn me() -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let f = tempfile::NamedTempFile::new().unwrap();
            f.as_file().metadata().unwrap().uid()
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    /// A loader over a real file, the way production reads it off-Windows.
    fn file_loader(path: std::path::PathBuf) -> Loader {
        Box::new(move || read_keyring_raw_from(Ok(None), Some((&path, me()))))
    }

    #[test]
    fn an_absent_keyring_file_is_nothing_provisioned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        assert_eq!(
            read_keyring_raw_from(Ok(None), Some((&path, me()))),
            Ok(None)
        );
        // No file store at all (Windows) behaves the same.
        assert_eq!(read_keyring_raw_from(Ok(None), None), Ok(None));
    }

    #[test]
    fn a_valid_keyring_file_is_read_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let sk = SigningKey::from_bytes(&[71u8; 32]);
        std::fs::write(&path, ring_json("backend-1", &sk)).unwrap();

        let raw = read_keyring_raw_from(Ok(None), Some((&path, me()))).unwrap();
        let ring = parse_raw(raw.as_deref()).unwrap();
        assert_eq!(ring.kids().collect::<Vec<_>>(), vec!["backend-1"]);
    }

    #[test]
    fn a_malformed_keyring_file_is_present_but_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        // One good entry and one bad: the strict parse rejects the whole ring.
        let sk = SigningKey::from_bytes(&[72u8; 32]);
        std::fs::write(
            &path,
            format!(
                r#"[{{"kid":"ok","public_key":"{}"}},{{"kid":"bad","public_key":"not-base64!"}}]"#,
                b64(sk.verifying_key().as_bytes())
            ),
        )
        .unwrap();
        let raw = read_keyring_raw_from(Ok(None), Some((&path, me()))).unwrap();
        assert!(raw.is_some(), "present, so not reported as absent");
        assert!(parse_raw(raw.as_deref()).is_err());
    }

    #[test]
    fn an_empty_keyring_file_is_an_error_but_an_empty_array_is_a_ring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        std::fs::write(&path, "  \n").unwrap();
        assert!(read_keyring_raw_from(Ok(None), Some((&path, me()))).is_err());
        std::fs::write(&path, "[]").unwrap();
        let raw = read_keyring_raw_from(Ok(None), Some((&path, me()))).unwrap();
        assert!(parse_raw(raw.as_deref()).unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_keyring_file_is_an_error_not_absence() {
        // Injected so it holds on every OS and when the suite runs as root.
        let path = std::path::Path::new("/etc/kanade/command-keys.json");
        let denied = Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(interpret_file_read(path, denied).is_err());
        let gone = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(interpret_file_read(path, gone), Ok(None));
    }

    #[cfg(unix)]
    #[test]
    fn a_keyring_file_others_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let sk = SigningKey::from_bytes(&[73u8; 32]);
        std::fs::write(&path, ring_json("backend-1", &sk)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_keyring_raw_from(Ok(None), Some((&path, me()))).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_keyring_raw_from(Ok(None), Some((&path, me()))).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_keyring_file_owned_by_someone_else_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let sk = SigningKey::from_bytes(&[82u8; 32]);
        std::fs::write(&path, ring_json("backend-1", &sk)).unwrap();
        // Mode is fine (not group/other-writable); only the owner differs.
        assert!(read_keyring_raw_from(Ok(None), Some((&path, me() + 1))).is_err());
        assert!(read_keyring_raw_from(Ok(None), Some((&path, me()))).is_ok());
    }

    #[test]
    fn the_registry_wins_over_the_file_and_a_registry_error_does_not_fall_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let file_key = SigningKey::from_bytes(&[74u8; 32]);
        std::fs::write(&path, ring_json("from-file", &file_key)).unwrap();

        // Registry holds a value: it is used, the file is ignored.
        let reg = Ok(Some("[]".to_string()));
        assert_eq!(
            read_keyring_raw_from(reg, Some((&path, me()))),
            Ok(Some("[]".to_string()))
        );
        // Registry has nothing: the file is used.
        assert_eq!(
            read_keyring_raw_from(Ok(None), Some((&path, me()))).unwrap(),
            Some(ring_json("from-file", &file_key))
        );
        // Registry failed: that is an error, not licence to use the file.
        assert!(
            read_keyring_raw_from(Err("registry unavailable".into()), Some((&path, me()))).is_err()
        );
    }

    #[test]
    fn a_file_keyring_verifies_and_is_listed_like_the_registry_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let sk = SigningKey::from_bytes(&[75u8; 32]);
        std::fs::write(&path, ring_json("backend-1", &sk)).unwrap();
        let v = Verifier::with_loader("PC1".into(), test_dir(), file_loader(path));

        let now = 1_700_000_000_000i64;
        assert_eq!(
            v.observe_at(b"p", &sign(&sk, "backend-1", b"p", now), "r1", now),
            Outcome::Verified
        );
        assert_eq!(v.trusted_keys(), vec![reported("backend-1", &sk)]);
    }

    #[test]
    fn a_key_added_to_the_file_after_boot_takes_effect_on_an_unknown_kid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let v = Verifier::with_loader("PC1".into(), test_dir(), file_loader(path.clone()));
        let sk = SigningKey::from_bytes(&[76u8; 32]);
        let now = 1_700_000_000_000i64;
        let t = Instant::now() + RELOAD_MIN_INTERVAL * 2;
        let signed = sign(&sk, "backend-1", b"p", now);
        assert_eq!(
            v.classify(b"p", &signed, "r1", now, t),
            Outcome::Unprovisioned
        );

        std::fs::write(&path, ring_json("backend-1", &sk)).unwrap();
        assert_eq!(
            v.classify(b"p", &signed, "r2", now, t + RELOAD_MIN_INTERVAL * 2),
            Outcome::Verified
        );
    }

    #[test]
    fn revoking_a_key_in_the_file_takes_effect_on_the_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let backend = SigningKey::from_bytes(&[77u8; 32]);
        let leaked = SigningKey::from_bytes(&[78u8; 32]);
        let mut both = backend_ring("backend-1", &backend);
        both.insert(
            "leaked",
            leaked.verifying_key(),
            KeyPolicy::backend("leaked"),
        );
        std::fs::write(&path, ring_to_json(&both).unwrap()).unwrap();
        let v = Verifier::with_loader("PC1".into(), test_dir(), file_loader(path.clone()));
        assert_eq!(v.trusted_keys().len(), 2);

        std::fs::write(&path, ring_json("backend-1", &backend)).unwrap();
        let (kids, _) = v.refresh_and_report();
        assert_eq!(kids, vec![reported("backend-1", &backend)]);

        // Removing the file is a deliberate revocation of everything.
        std::fs::remove_file(&path).unwrap();
        let (kids, _) = v.refresh_and_report();
        assert!(kids.is_empty());
    }

    #[test]
    fn a_broken_keyring_file_keeps_the_working_ring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-keys.json");
        let sk = SigningKey::from_bytes(&[79u8; 32]);
        std::fs::write(&path, ring_json("backend-1", &sk)).unwrap();
        let v = Verifier::with_loader("PC1".into(), test_dir(), file_loader(path.clone()));

        std::fs::write(&path, "[{\"kid\":\"half-writ").unwrap();
        let (kids, _) = v.refresh_and_report();
        assert_eq!(kids, vec![reported("backend-1", &sk)]);
    }

    #[test]
    fn the_installer_check_applies_the_deploy_script_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        let sk = SigningKey::from_bytes(&[81u8; 32]);
        let pk = b64(sk.verifying_key().as_bytes());
        let check = |body: &str| {
            std::fs::write(&path, body).unwrap();
            check_keyring_file(&path)
        };
        assert_eq!(
            check(&format!(r#"[{{"kid":"a","public_key":"{pk}"}}]"#)),
            Ok(vec!["a".to_string()])
        );
        assert!(check("{}").is_err(), "not an array");
        assert!(check("[]").is_err(), "empty");
        assert!(check("nope").is_err(), "not JSON");
        assert!(check(&format!(r#"[{{"kid":20260728,"public_key":"{pk}"}}]"#)).is_err());
        assert!(check(&format!(r#"[{{"kid":" ","public_key":"{pk}"}}]"#)).is_err());
        assert!(check(r#"[{"kid":"a","public_key":""}]"#).is_err());
        assert!(
            check(&format!(
                r#"[{{"kid":"a","public_key":"{pk}"}},{{"kid":"a","public_key":"{pk}"}}]"#
            ))
            .is_err(),
            "duplicate kid"
        );
        assert!(check(r#"[{"kid":"a","public_key":"not-a-key"}]"#).is_err());
        for bad in ["0", "-5", "\"900\"", "1.5"] {
            assert!(
                check(&format!(
                    r#"[{{"kid":"a","public_key":"{pk}","max_age_secs":{bad}}}]"#
                ))
                .is_err(),
                "max_age_secs {bad}"
            );
        }
        assert!(check(&format!(r#"[{{"kid":"a","public_key":"{pk}","label":5}}]"#)).is_err());
        assert!(
            check(&format!(
                r#"[{{"kid":"a","public_key":"{pk}","max_age_secs":900}}]"#
            ))
            .is_ok()
        );
    }

    // ---- enforcement flag off-Windows ----

    #[test]
    fn the_enforce_file_turns_enforcement_on_only_for_truthy_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("require-signed-commands");
        for (content, want) in [
            ("1\n", true),
            ("true", true),
            (" YES ", true),
            ("0", false),
            ("", false),
            ("no", false),
        ] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(
                enforce_requested_from(None, Some((&path, me()))),
                want,
                "content {content:?}"
            );
        }
    }

    #[test]
    fn an_absent_or_unreadable_enforce_file_leaves_enforcement_off() {
        let dir = tempfile::tempdir().unwrap();
        // Absent, and a directory where a file should be (unreadable as text).
        assert!(!enforce_requested_from(
            None,
            Some((&dir.path().join("nope"), me()))
        ));
        assert!(!enforce_requested_from(None, Some((dir.path(), me()))));
        assert!(!enforce_requested_from(None, None));
    }

    #[cfg(unix)]
    #[test]
    fn an_enforce_file_others_can_rewrite_is_treated_as_enforcing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("require-signed-commands");
        // Owned by someone else: even "0" must not switch enforcement off.
        std::fs::write(&path, "0").unwrap();
        assert!(enforce_requested_from(None, Some((&path, me() + 1))));
        // Writable by group/other.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(enforce_requested_from(None, Some((&path, me()))));
        // Trusted again: the content decides.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!enforce_requested_from(None, Some((&path, me()))));
    }

    #[test]
    fn the_registry_flag_takes_precedence_over_the_enforce_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("require-signed-commands");
        std::fs::write(&path, "0").unwrap();
        assert!(enforce_requested_from(
            Some("1".into()),
            Some((&path, me()))
        ));
        std::fs::write(&path, "1").unwrap();
        assert!(!enforce_requested_from(
            Some("0".into()),
            Some((&path, me()))
        ));
    }

    #[test]
    fn a_requested_flag_with_an_empty_ring_still_declines_to_enforce() {
        let v =
            Verifier::with_loader_and_policy("PC1".into(), test_dir(), Box::new(|| Ok(None)), true);
        assert!(!v.enforcing_now());
        let sk = SigningKey::from_bytes(&[80u8; 32]);
        let v = enforcing_with(backend_ring("backend-1", &sk));
        assert!(v.enforcing_now());
    }

    // ---- v2 envelope admission ----

    mod envelope {
        use super::*;
        use chrono::{DateTime, TimeZone, Utc};
        use kanade_shared::signing::Signer;
        use kanade_shared::wire::{RunAs, Shell, Staleness, sign_envelope};

        fn t(secs: i64) -> DateTime<Utc> {
            Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
        }

        fn command() -> Command {
            Command {
                id: "echo".into(),
                version: "1.0.0".into(),
                request_id: "req-1".into(),
                exec_id: None,
                shell: Shell::Sh,
                script: "echo hi".into(),
                script_object: None,
                script_object_sha256: None,
                timeout_secs: 30,
                bypass_local_limit: false,
                jitter_secs: None,
                run_as: RunAs::System,
                cwd: None,
                deadline_at: None,
                staleness: Staleness::Cached,
                emit: None,
                check: None,
                collect: None,
                retry: None,
                finalize: None,
            }
        }

        fn setup(enforcing: bool) -> (Verifier, Signer) {
            let sk = SigningKey::from_bytes(&[5u8; 32]);
            let ring = backend_ring("k", &sk);
            let v = if enforcing {
                enforcing_with(ring)
            } else {
                verifier_with(ring)
            };
            (v, Signer::new(sk, "k"))
        }

        fn admit(v: &Verifier, body: &[u8], h: &SigHeaders, now: DateTime<Utc>) -> Admission {
            v.admit_at(body, h, "commands.pc.PC1", now, Instant::now())
        }

        fn last(v: &Verifier) -> Option<Outcome> {
            *lock(&v.last)
        }

        #[test]
        fn a_valid_envelope_for_this_host_runs_with_its_deadline_in_either_mode() {
            for enforcing in [false, true] {
                let (v, s) = setup(enforcing);
                let (body, h) = sign_envelope(&s, "PC1", t(3600), command(), t(0)).unwrap();
                match admit(&v, &body, &h, t(5)) {
                    Admission::Run {
                        cmd,
                        envelope_deadline,
                    } => {
                        assert_eq!(cmd.request_id, "req-1");
                        assert_eq!(envelope_deadline, Some(t(3600)));
                    }
                    other => panic!("enforcing={enforcing}: {other:?}"),
                }
                assert_eq!(last(&v), Some(Outcome::Verified));
            }
        }

        #[test]
        fn an_envelope_for_another_host_is_refused_without_a_result_even_unenforced() {
            for enforcing in [false, true] {
                let (v, s) = setup(enforcing);
                let (body, h) = sign_envelope(&s, "PC2", t(3600), command(), t(0)).unwrap();
                assert!(matches!(admit(&v, &body, &h, t(5)), Admission::Refused));
                assert_eq!(last(&v), Some(Outcome::Misaddressed));
            }
        }

        #[test]
        fn clock_refusals_are_reported_apart_from_tampering() {
            let (v, s) = setup(false);
            let (body, h) = sign_envelope(&s, "PC1", t(100), command(), t(0)).unwrap();
            assert!(matches!(admit(&v, &body, &h, t(200)), Admission::Refused));
            assert_eq!(last(&v), Some(Outcome::Expired));

            let (body, h) = sign_envelope(&s, "PC1", t(10_000), command(), t(0)).unwrap();
            assert!(matches!(
                admit(&v, &body, &h, t(-4_000)),
                Admission::Refused
            ));
            assert_eq!(last(&v), Some(Outcome::ClockAhead));

            // Tampering is still a signature failure, not a clock one.
            let tampered = String::from_utf8(body).unwrap().replace("PC1", "PC9");
            assert!(matches!(
                admit(&v, tampered.as_bytes(), &h, t(5)),
                Admission::Refused
            ));
            assert_eq!(last(&v), Some(Outcome::Invalid));
        }

        #[test]
        fn an_unsigned_envelope_is_refused_even_when_not_enforcing() {
            let (v, s) = setup(false);
            let (body, _) = sign_envelope(&s, "PC1", t(3600), command(), t(0)).unwrap();
            assert!(matches!(
                admit(&v, &body, &SigHeaders::default(), t(5)),
                Admission::Refused
            ));
            assert_eq!(last(&v), Some(Outcome::Unsigned));
        }

        #[test]
        fn an_unrecognised_kind_is_refused_and_never_falls_back_to_legacy() {
            let (v, s) = setup(false);
            let cmd = serde_json::to_string(&command()).unwrap();
            for kind in [r#""kanade.command.v9""#, "null", "3", r#""""#] {
                // A body that would also parse as a legacy command if the kind
                // were ignored.
                let mut obj: serde_json::Value = serde_json::from_str(&cmd).unwrap();
                obj["kind"] = serde_json::from_str(kind).unwrap();
                let body = serde_json::to_vec(&obj).unwrap();
                let h = s.headers(&body, t(0).timestamp_millis());
                assert!(
                    matches!(admit(&v, &body, &h, t(5)), Admission::Refused),
                    "kind {kind}"
                );
            }
            // Refusing a kind is not a signing-state report: it is reachable by
            // anyone who can put bytes on the subject.
            assert_eq!(last(&v), None);
        }

        #[test]
        fn legacy_commands_are_handled_exactly_as_before() {
            let body = serde_json::to_vec(&command()).unwrap();

            // Not enforcing: unsigned runs, with no envelope deadline.
            let (v, s) = setup(false);
            match admit(&v, &body, &SigHeaders::default(), t(5)) {
                Admission::Run {
                    envelope_deadline, ..
                } => assert_eq!(envelope_deadline, None),
                other => panic!("{other:?}"),
            }
            assert_eq!(last(&v), Some(Outcome::Unsigned));
            // A signed legacy command for any recipient still verifies, however old.
            let h = s.headers(&body, t(-30 * 86_400).timestamp_millis());
            assert!(matches!(admit(&v, &body, &h, t(5)), Admission::Run { .. }));
            assert_eq!(last(&v), Some(Outcome::Verified));

            // Enforcing: unsigned is refused with a result to publish, as today.
            let (v, _) = setup(true);
            assert!(matches!(
                admit(&v, &body, &SigHeaders::default(), t(5)),
                Admission::RefusedLegacy { .. }
            ));

            // Not a command at all.
            assert!(matches!(
                admit(&v, b"not json", &SigHeaders::default(), t(5)),
                Admission::Undecodable(_)
            ));
        }
    }
}
