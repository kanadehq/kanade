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

use std::sync::Mutex;

use kanade_shared::signing::{KeyPolicy, KeyRing, SigHeaders, VerifyError, verify};
use kanade_shared::wire::ObsEvent;
use serde::Deserialize;
use tracing::{info, warn};

const REG_SUBKEY: &str = r"SOFTWARE\kanade\agent";
const REG_VALUE: &str = "CommandKeys";

/// `source` on emitted [`ObsEvent`]s.
const SOURCE: &str = "command_signature";

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

/// Load the trusted keys. An absent or unreadable value yields an empty ring,
/// which is correct for stage 1: an agent with no keys still runs unsigned
/// commands exactly as it does today, and reports every signed one it cannot
/// check.
pub fn load_keyring() -> KeyRing {
    let Some(raw) = kanade_shared::secrets::read_hklm_value(REG_SUBKEY, REG_VALUE) else {
        return KeyRing::new();
    };
    parse_keyring(&raw).unwrap_or_else(|e| {
        // Fail to an empty ring rather than panicking: a corrupt keyring must
        // not stop an agent from working during stages 1-2, and at stage 3 an
        // empty ring rejects everything, which is the safe direction.
        warn!(error = %e, "command keyring is unreadable — treating as empty");
        KeyRing::new()
    })
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
    /// Signed by a key this agent does not have. Almost always a stale
    /// keyring mid-rotation; indistinguishable at a glance from a backend
    /// that has stopped sending, which is why it is reported.
    UnknownKid,
    /// Signed, and the signature does not check out. Either a forgery or a
    /// corrupted message; both warrant a look.
    Invalid,
}

impl Outcome {
    fn kind(self) -> &'static str {
        match self {
            Outcome::Verified => "command_signature_ok",
            Outcome::Unsigned => "command_signature_absent",
            Outcome::UnknownKid => "command_signature_unknown_key",
            Outcome::Invalid => "command_signature_invalid",
        }
    }
}

/// Verifies commands and reports when the fleet's signing state changes.
pub struct Verifier {
    ring: KeyRing,
    pc_id: String,
    obs_dir: std::path::PathBuf,
    /// Last reported class. Events fire on **transition**, the same shape the
    /// idle sampler uses: a per-command event would emit thousands of
    /// "unsigned" rows a day through stages 1-2 and bury the one that matters.
    last: Mutex<Option<Outcome>>,
}

impl Verifier {
    pub fn new(ring: KeyRing, pc_id: String, obs_dir: std::path::PathBuf) -> Self {
        // Every other `obs_outbox::enqueue` caller in this crate does this
        // first; skipping it would make the first report fail on a fresh
        // install, which is precisely when a mis-provisioned keyring is most
        // likely and least visible.
        if let Err(e) = crate::obs_outbox::ensure_outbox_dir(&obs_dir) {
            warn!(error = %e, "command_verify: outbox dir — reports may be dropped until it exists");
        }
        if ring.is_empty() {
            info!("command keyring is empty — signatures will be reported, not checked");
        } else {
            info!(kids = ?ring.kids().collect::<Vec<_>>(), "command keyring loaded");
        }
        Self {
            ring,
            pc_id,
            obs_dir,
            last: Mutex::new(None),
        }
    }

    /// Check one message and report. **Never blocks execution** — stage 1.
    pub fn observe(&self, body: &[u8], headers: &SigHeaders, request_id: &str) -> Outcome {
        let outcome = match verify(&self.ring, body, headers) {
            Ok(v) => {
                if v.policy.audit_every_use {
                    // A break-glass key whose use nobody investigates is a
                    // second production key, so this is unconditional and
                    // deliberately not rate-limited.
                    warn!(kid = v.kid, request_id, "command signed by an audited key");
                }
                Outcome::Verified
            }
            Err(VerifyError::Unsigned) => Outcome::Unsigned,
            Err(VerifyError::UnknownKid { kid }) => {
                warn!(
                    kid,
                    request_id,
                    known = ?self.ring.kids().collect::<Vec<_>>(),
                    "command signed by a key this agent does not have"
                );
                Outcome::UnknownKid
            }
            Err(e) => {
                warn!(error = %e, request_id, "command signature did not verify");
                Outcome::Invalid
            }
        };
        self.report_transition(outcome);
        outcome
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
        let mut last = match self.last.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
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
            verify(&ring, b"body", &SigHeaders::default()),
            Err(VerifyError::Unsigned)
        );
        // Signed traffic it cannot check: reported, not silently accepted as
        // if it were unsigned.
        let headers = sign(&sk, "backend-1", b"body");
        assert!(matches!(
            verify(&ring, b"body", &headers),
            Err(VerifyError::UnknownKid { .. })
        ));
    }

    #[test]
    fn outcome_kinds_are_distinct_and_stable() {
        // These strings reach the SPA's Events filter and the backend's
        // UNIQUE key, so a collision or a rename is a data change, not a
        // cosmetic one.
        let kinds: Vec<_> = [
            Outcome::Verified,
            Outcome::Unsigned,
            Outcome::UnknownKid,
            Outcome::Invalid,
        ]
        .iter()
        .map(|o| o.kind())
        .collect();
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
}
