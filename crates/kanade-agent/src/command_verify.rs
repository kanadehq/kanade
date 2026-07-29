//! Agent-side command provenance check (#1165, rollout stage 1).
//!
//! Verifies the backend's signature on every wire `Command` and **reports the
//! outcome without acting on it**. Nothing is rejected here yet; that is stage
//! 3, and the ordering is deliberate — capability first, enforcement last, so
//! every step of the rollout is reversible (the same shape #1159 used for
//! per-role NATS credentials).
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
//! So [`read_keyring`] separates "nothing provisioned" (`Ok(empty)` — a real
//! state an operator can intend) from "provisioned and unusable" (`Err`), and
//! only the former replaces a live ring.
//!
//! ## What stage 3 has to add here
//!
//! With enforcement on, the moment a command is about to be **rejected** for an
//! unknown key is exactly the moment a skipped reload stops being free. Stage 3
//! must force a reload before rejecting, ignoring [`RELOAD_MIN_INTERVAL`] —
//! otherwise a key that landed seconds ago is still refused, and the rate limit
//! turns from an I/O bound into a rejection bug.
//!
//! ## If the keyring source stops being local
//!
//! The reload runs synchronously on the async command path, which is fine only
//! because the registry is local and memory-mapped: microseconds, at most once
//! per interval per machine. Move the ring to a network fetch, a remote share
//! or a KV read and that inverts — it then belongs on `spawn_blocking`, and the
//! `last_reload` guard held across the load (which is what collapses two
//! concurrent misses into one read) has to be reworked around an async-aware
//! lock rather than simply dropped, or the dedup it provides is lost.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use kanade_shared::signing::{KeyPolicy, KeyRing, SigHeaders, VerifyError, verify};
use kanade_shared::wire::ObsEvent;
use serde::Deserialize;
use tracing::{info, warn};

const REG_SUBKEY: &str = r"SOFTWARE\kanade\agent";
const REG_VALUE: &str = "CommandKeys";

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
fn read_keyring() -> Result<KeyRing, String> {
    let Some(raw) = kanade_shared::secrets::read_hklm_value(REG_SUBKEY, REG_VALUE) else {
        return Ok(KeyRing::new());
    };
    parse_keyring(&raw)
}

fn parse_keyring(raw: &str) -> Result<KeyRing, String> {
    use base64::Engine;
    let entries: Vec<KeyEntry> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let mut ring = KeyRing::new();
    for e in entries {
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
}

impl Outcome {
    /// Every variant, kept **here** rather than in the test that consumes it.
    ///
    /// The uniqueness guard on [`Outcome::kind`] is only as good as this list,
    /// and a list living in a test module is one a new variant gets added
    /// without — which is exactly what happened when `Unprovisioned` was added.
    /// Sitting against the enum, it is in the diff you are already editing.
    #[cfg(test)]
    const ALL: [Outcome; 6] = [
        Outcome::Verified,
        Outcome::Unsigned,
        Outcome::Unprovisioned,
        Outcome::UnknownKid,
        Outcome::Invalid,
        Outcome::Stale,
    ];

    fn kind(self) -> &'static str {
        match self {
            Outcome::Verified => "command_signature_ok",
            Outcome::Unsigned => "command_signature_absent",
            Outcome::Unprovisioned => "command_signature_unprovisioned",
            Outcome::UnknownKid => "command_signature_unknown_key",
            Outcome::Invalid => "command_signature_invalid",
            Outcome::Stale => "command_signature_stale",
        }
    }
}

/// What a reload attempt did, for the log line an operator reads when a
/// machine will not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reload {
    /// The ring was replaced with what the store holds.
    Done,
    /// Inside [`RELOAD_MIN_INTERVAL`] of the last attempt — we did not look.
    RateLimited,
    /// We looked and could not use what we found. The previous ring is kept.
    Failed,
}

impl Reload {
    fn as_str(self) -> &'static str {
        match self {
            Reload::Done => "reloaded",
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
/// [`read_keyring`] so the reload path is reachable from a test — the registry
/// returns nothing on non-Windows, so a test that went through it would assert
/// nothing at all on CI, which is where this needs to hold.
type Loader = Box<dyn Fn() -> Result<KeyRing, String> + Send + Sync>;

/// Verifies commands and reports when the fleet's signing state changes.
pub struct Verifier {
    /// Behind a lock because an `UnknownKid` replaces it in place — see the
    /// module doc. `Mutex` rather than `RwLock`: reads are already serialized
    /// by the command path being one message at a time per subscription, and
    /// the lock is held for a signature verify (tens of microseconds).
    ring: Mutex<KeyRing>,
    loader: Loader,
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
        Self::with_loader(pc_id, obs_dir, Box::new(read_keyring))
    }

    /// The ring's source is a single argument so the initial load and every
    /// reload cannot drift apart — the caller can no longer hand in one ring
    /// and have it silently refreshed from somewhere else.
    fn with_loader(pc_id: String, obs_dir: std::path::PathBuf, loader: Loader) -> Self {
        // Every other `obs_outbox::enqueue` caller in this crate does this
        // first; skipping it would make the first report fail on a fresh
        // install, which is precisely when a mis-provisioned keyring is most
        // likely and least visible.
        if let Err(e) = crate::obs_outbox::ensure_outbox_dir(&obs_dir) {
            warn!(error = %e, "command_verify: outbox dir — reports may be dropped until it exists");
        }
        let ring = loader().unwrap_or_else(|e| {
            // No earlier ring to lose at construction, so degrading is safe
            // here in a way it is not on the reload path.
            warn!(error = %e, "command keyring is unreadable — treating as empty");
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
        Self {
            ring: Mutex::new(ring),
            loader,
            last_reload: Mutex::new(Instant::now()),
            pc_id,
            obs_dir,
            last: Mutex::new(None),
        }
    }

    /// Check one message and report.
    ///
    /// **Never withholds a command from running** — stage 1 is observational,
    /// so every outcome here, including a failed verification, still lets the
    /// command execute. (That is a statement about authorisation, not about
    /// executor scheduling: this is a synchronous call and may do one local
    /// registry read per [`RELOAD_MIN_INTERVAL`] — see the module doc.)
    pub fn observe(&self, body: &[u8], headers: &SigHeaders, request_id: &str) -> Outcome {
        self.observe_at(
            body,
            headers,
            request_id,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    /// [`Verifier::observe`] with the clock injected, so the freshness branch
    /// is reachable from a test.
    fn observe_at(
        &self,
        body: &[u8],
        headers: &SigHeaders,
        request_id: &str,
        now_ms: i64,
    ) -> Outcome {
        let outcome = self.classify(body, headers, request_id, now_ms, Instant::now());
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
        match self.check(body, headers, request_id, now_ms) {
            Ok(outcome) => outcome,
            // The one outcome a stale in-memory ring can explain. Everything
            // else — unsigned, malformed, bad signature, stale — means the same
            // thing whatever keys we hold, so it must not reach the store: the
            // trigger is reachable by anyone who can put bytes on a command
            // subject.
            Err(kid) => {
                let reload = self.reload_if_due(now);
                if reload != Reload::Done {
                    return self.report_missing(&kid, request_id, reload.as_str());
                }
                match self.check(body, headers, request_id, now_ms) {
                    Ok(outcome) => {
                        info!(
                            kid,
                            request_id, "keyring reload resolved a previously unknown key"
                        );
                        outcome
                    }
                    Err(kid) => self.report_missing(&kid, request_id, "reloaded"),
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

    /// Classify and log a key we still do not hold after doing what we can.
    fn report_missing(&self, kid: &str, request_id: &str, reload: &str) -> Outcome {
        let ring = lock(&self.ring);
        if ring.is_empty() {
            warn!(
                kid,
                request_id,
                reload,
                "command is signed but this agent holds no keys — provision \
                 HKLM\\SOFTWARE\\kanade\\agent\\CommandKeys"
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
    fn reload_if_due(&self, now: Instant) -> Reload {
        let mut last = lock(&self.last_reload);
        // `checked_duration_since` rather than subtraction: an `Instant` from
        // before the recorded one would panic on the underflow, and a test (or
        // a future caller) passing a non-monotonic clock should not take the
        // agent down.
        if now.checked_duration_since(*last).unwrap_or_default() < RELOAD_MIN_INTERVAL {
            return Reload::RateLimited;
        }
        // Consumed even when the load fails: a corrupt value plus a stream of
        // unknown-key commands would otherwise be one read per command, which
        // is the case the floor exists for.
        *last = now;
        // Held across the load deliberately: two command paths hitting an
        // unknown key at once would otherwise both read the store.
        match (self.loader)() {
            Ok(fresh) => {
                *lock(&self.ring) = fresh;
                Reload::Done
            }
            // Keep what we have. A reload can only ever improve the ring or
            // leave it alone — never destroy a working one — which is what
            // makes a skipped or failed reload a delay rather than an outage.
            Err(e) => {
                warn!(
                    error = %e,
                    "keyring reload failed — keeping the keys already loaded"
                );
                Reload::Failed
            }
        }
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

    /// A verifier whose ring never changes — the pre-reload behaviour.
    fn verifier_with(ring: KeyRing) -> Verifier {
        Verifier::with_loader("PC1".into(), test_dir(), Box::new(move || Ok(ring.clone())))
    }

    fn backend_ring(kid: &str, sk: &SigningKey) -> KeyRing {
        let mut r = KeyRing::new();
        r.insert(kid, sk.verifying_key(), KeyPolicy::backend("backend"));
        r
    }

    /// A ring that is empty until `provision()` is called, counting how many
    /// times it was read — the shape of a machine whose key arrives while the
    /// agent is already running.
    #[derive(Clone)]
    struct Store {
        inner: std::sync::Arc<Mutex<(Result<KeyRing, String>, usize)>>,
    }

    impl Default for Store {
        fn default() -> Self {
            Self {
                inner: std::sync::Arc::new(Mutex::new((Ok(KeyRing::new()), 0))),
            }
        }
    }

    impl Store {
        fn provision(&self, ring: KeyRing) {
            lock(&self.inner).0 = Ok(ring);
        }
        /// What a partially-written `CommandKeys` value looks like from here.
        fn corrupt(&self) {
            lock(&self.inner).0 = Err("expected value at line 1 column 3".into());
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
    fn an_empty_ring_reports_unsigned_and_unknown_but_never_verifies() {
        let ring = parse_keyring("[]").unwrap();
        assert!(ring.is_empty());
        let sk = SigningKey::from_bytes(&[3u8; 32]);

        // Unsigned traffic on an agent with no keys: normal, stage 1.
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

        let empty =
            Verifier::with_loader("PC1".into(), test_dir(), Box::new(|| Ok(KeyRing::new())));
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
}
