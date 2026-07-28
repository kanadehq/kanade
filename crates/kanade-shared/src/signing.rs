//! Command provenance: the backend signs, the agent verifies (#1165).
//!
//! The agent authorises a command **by the subject it arrived on** and runs it
//! without checking who wrote it. #1155 measured why that is not fixable with
//! credentials: `deliver_subject` is not authorization-checked, so a connected
//! party can place attacker-authored bytes on `commands.all` /
//! `commands.pc.<id>` through a push consumer, and subject permissions cannot
//! express a rule against it. Signing attacks a different axis — the agent
//! checks provenance regardless of how the bytes arrived.
//!
//! This is the code-signing model (signed packages, Authenticode), not a login
//! handshake. The asymmetry is the whole point: the **private** key lives only
//! on the backend, and agents hold only **public** verify keys, which are not
//! secrets. Compromising one of hundreds of endpoints therefore yields nothing
//! that can command another — the endpoint holds no key capable of authoring a
//! valid command.
//!
//! # Where the signature travels
//!
//! In **NATS headers**, over the message body's exact bytes — not in an
//! envelope wrapping the body.
//!
//! Both are detached signatures and both sidestep canonicalisation (verify the
//! received bytes, *then* deserialize). Headers win on migration: the body
//! stays a bare serialized `Command`, so an agent that predates this feature
//! parses a signed message exactly as it parses an unsigned one and is
//! unaffected by the rollout. An envelope would change the body's shape, which
//! means every agent must understand both shapes for the whole of stages 1–2 —
//! a dual-parse path across a fleet-wide upgrade window, to buy nothing.
//!
//! The frame plane already does this (#1140: metadata in headers, raw payload)
//! for the same reason: don't wrap the body when the transport has a place for
//! metadata.
//!
//! # Multiple signers from the start
//!
//! [`SIG_KID`] is populated and checked from the first release, even while
//! only one key exists. Adding a key id later is a wire break across the whole
//! fleet; reserving it now costs nothing, and it buys two things:
//!
//! * **Rotation is not a separate mechanism.** Rotating = two valid kids for a
//!   window, which is the same code path as having two signers. A rotation
//!   procedure that shares its implementation with everyday operation is one
//!   that still works the day it is needed.
//! * **The recovery-path decision can wait.** `kanade run` publishes its own
//!   commands (`crates/kanade/src/cmd/run.rs`) and is the documented
//!   backend-down recovery route; whether it gets a break-glass key, routes
//!   through the backend, or is retired does not have to be settled before the
//!   wire format ships.
//!
//! A [`KeyRing`] therefore maps `kid → (key, policy)` rather than holding a
//! bare set of keys. Policy is what makes more keys *safer* rather than merely
//! wider: a break-glass key should not silently carry the same authority as
//! the backend's.

use std::collections::BTreeMap;
use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Header carrying the base64 (standard, padded) Ed25519 signature over the
/// message body.
pub const SIG: &str = "Kanade-Sig";
/// Header naming which key signed it. Present from the first release; see the
/// module doc on why it is not deferred.
pub const SIG_KID: &str = "Kanade-Sig-Kid";
/// Header naming the algorithm. Ed25519 today; present so a future migration
/// is a value change rather than a format change.
pub const SIG_ALG: &str = "Kanade-Sig-Alg";
/// Header carrying when the message was signed, as decimal milliseconds since
/// the Unix epoch. **Covered by the signature** — see [`signed_material`].
pub const SIG_AT: &str = "Kanade-Sig-At";

/// The only algorithm currently emitted or accepted.
pub const ALG_ED25519: &str = "ed25519";

/// The bytes a signature actually covers: the signing time, then the message.
///
/// The timestamp is **inside** the signed material, unlike [`SIG_KID`], and
/// the asymmetry is easy to get backwards. A rewritten `kid` can only select a
/// key under which verification fails, so it is safe outside. A rewritten
/// timestamp would let anyone replay a captured message by making it look
/// fresh again — which is the entire freshness bound, defeated by editing one
/// header. So it is signed.
///
/// The prefix is fixed-width rather than delimited so there is no
/// concatenation ambiguity to reason about: no timestamp encoding can be
/// chosen such that one `(at, body)` pair produces the same bytes as another.
/// Keeping it a prefix rather than a field also leaves the `Command` wire type
/// untouched — agent-authored in-process commands would otherwise carry a
/// field that means nothing for them.
pub fn signed_material(at_ms: i64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&at_ms.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// What a key is allowed to authorise.
///
/// Exists from the first release with a single variant in practical use,
/// because a keyring without policy is just more keys that can do everything —
/// strictly worse than one key. Adding the concept later would mean auditing
/// every existing key's authority retroactively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPolicy {
    /// Human-facing note for logs and audit ("backend", "break-glass").
    pub label: String,
    /// Reject a signature older than this. `None` = no freshness bound.
    ///
    /// **`None` is required for the ordinary signer, not merely convenient.**
    /// The agent's JetStream replay path deliberately redelivers the most
    /// recent retained command per subject, kept for 7 days, so an agent
    /// reconnecting after a week legitimately receives week-old commands. A
    /// bound on the backend key would reject exactly that.
    ///
    /// A break-glass key sets a short bound, and there the bound is doing real
    /// work rather than being belt-and-braces. Replay dedup is keyed on
    /// `request_id` in an in-memory cache, which is **empty on first boot** —
    /// so without a freshness check, a machine booting for the first time
    /// would execute a week-old emergency command that no dedup can catch.
    pub max_age: Option<Duration>,
    /// Emit an audit record on **every** use of this key, including failed
    /// verifications.
    ///
    /// A break-glass credential whose use nobody investigates is a second
    /// production key with extra steps, so the flag lives next to the key
    /// rather than in a call site that can forget it.
    pub audit_every_use: bool,
}

impl KeyPolicy {
    /// The ordinary signer: no extra freshness bound, no per-use audit (the
    /// command itself is already audited).
    pub fn backend(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            max_age: None,
            audit_every_use: false,
        }
    }

    /// A deliberately-guarded key: short-lived signatures, every use recorded.
    pub fn break_glass(label: impl Into<String>, max_age: Duration) -> Self {
        Self {
            label: label.into(),
            max_age: Some(max_age),
            audit_every_use: true,
        }
    }
}

/// The public keys an agent trusts, keyed by `kid`.
#[derive(Debug, Clone, Default)]
pub struct KeyRing {
    keys: BTreeMap<String, (VerifyingKey, KeyPolicy)>,
}

impl KeyRing {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, kid: impl Into<String>, key: VerifyingKey, policy: KeyPolicy) {
        self.keys.insert(kid.into(), (key, policy));
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Look a key up, yielding the ring's own `kid` string so a verified
    /// result borrows from the trusted ring rather than from the attacker-
    /// supplied header it was matched against.
    pub fn get(&self, kid: &str) -> Option<(&str, &VerifyingKey, &KeyPolicy)> {
        self.keys
            .get_key_value(kid)
            .map(|(k, (key, policy))| (k.as_str(), key, policy))
    }

    /// Every `kid` on the ring, for reporting which keys an agent holds.
    pub fn kids(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(String::as_str)
    }
}

/// Why a message was not accepted as backend-authored.
///
/// Deliberately distinguishes "carries no signature" from every other case.
/// During stages 1–2 of the rollout `Unsigned` is expected and benign, while
/// the rest mean something is wrong — conflating them would hide a real
/// failure inside a normal one for the whole migration window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// No signature headers at all. Normal until enforcement is switched on.
    Unsigned,
    /// Signed, but the `kid` is not on this agent's ring.
    ///
    /// The operationally dangerous case: an agent that never received a new
    /// key looks, from the operator's side, exactly like a backend that is not
    /// sending commands — and a mid-rotation fleet is full of agents in that
    /// state. Callers must surface this, not just log it locally.
    UnknownKid { kid: String },
    /// Signed with an algorithm this build does not implement.
    UnsupportedAlg { alg: String },
    /// Signature header present but not decodable as a signature.
    Malformed(String),
    /// Cryptographically invalid for the bytes received.
    BadSignature { kid: String },
    /// Valid, by a key whose policy bounds how old a signature may be, and
    /// this one is older. Distinct from [`VerifyError::BadSignature`] because
    /// the signature is genuine — what failed is the policy, and the two want
    /// different responses.
    Stale {
        kid: String,
        age_ms: i64,
        max_age_ms: u128,
    },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Unsigned => write!(f, "no signature"),
            VerifyError::UnknownKid { kid } => {
                write!(
                    f,
                    "signed by unknown key id {kid} — is this agent's keyring current?"
                )
            }
            VerifyError::UnsupportedAlg { alg } => {
                write!(f, "unsupported signature algorithm {alg}")
            }
            VerifyError::Malformed(e) => write!(f, "malformed signature: {e}"),
            VerifyError::BadSignature { kid } => {
                write!(f, "signature by {kid} does not match these bytes")
            }
            VerifyError::Stale {
                kid,
                age_ms,
                max_age_ms,
            } => write!(
                f,
                "signature by {kid} is {age_ms}ms old, past its {max_age_ms}ms bound"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// A verified message: which key vouched for it, and under what policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified<'a> {
    pub kid: &'a str,
    pub policy: &'a KeyPolicy,
}

/// The signature headers carried alongside a message body.
///
/// Extracted from the transport by the caller so this module stays free of a
/// NATS dependency and is testable without a broker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SigHeaders {
    pub sig_b64: Option<String>,
    pub kid: Option<String>,
    pub alg: Option<String>,
    /// Decimal milliseconds since the Unix epoch, as sent. Parsed rather than
    /// trusted — it is covered by the signature, so a value that does not
    /// reconstruct the signed material simply fails verification.
    pub at_ms: Option<String>,
}

impl SigHeaders {
    /// True only when the message makes **no signing claim at all**.
    ///
    /// Any one of these headers means the sender is asserting the message is
    /// signed, so a partial set is a broken claim rather than an absent one.
    /// `alg` counts for the same reason `kid` does: the rule has to be "no
    /// signing headers at all", not "none of the two I happened to think of",
    /// or stripping whichever header is unchecked reclassifies a malformed
    /// message as an unsigned one — and unsigned is the accepted case for the
    /// whole of stages 1-2.
    pub fn is_absent(&self) -> bool {
        self.sig_b64.is_none() && self.kid.is_none() && self.alg.is_none() && self.at_ms.is_none()
    }
}

/// Verify `body` against the signature headers using `ring`.
///
/// Verification is over the **exact received bytes**; deserialization happens
/// afterwards, at the caller. That ordering is what removes canonicalisation
/// from the problem — there is no need for serde to produce byte-identical
/// output on both sides, only for the bytes to arrive unchanged.
pub fn verify<'a>(
    ring: &'a KeyRing,
    body: &[u8],
    headers: &SigHeaders,
    now_ms: i64,
) -> Result<Verified<'a>, VerifyError> {
    if headers.is_absent() {
        return Err(VerifyError::Unsigned);
    }
    // A signature with no kid cannot be attributed, so it is not "unsigned" —
    // it is a signed message we cannot route to a key.
    let kid = headers
        .kid
        .as_deref()
        .ok_or_else(|| VerifyError::Malformed("signature without a key id".into()))?;
    let sig_b64 = headers
        .sig_b64
        .as_deref()
        .ok_or_else(|| VerifyError::Malformed("key id without a signature".into()))?;
    let at_raw = headers
        .at_ms
        .as_deref()
        .ok_or_else(|| VerifyError::Malformed("signature without a signing time".into()))?;
    let at_ms: i64 = at_raw
        .parse()
        .map_err(|_| VerifyError::Malformed(format!("signing time {at_raw} is not a number")))?;

    // Absent alg means the original single-algorithm shape; anything else must
    // match exactly rather than being guessed at.
    if let Some(alg) = headers.alg.as_deref()
        && alg != ALG_ED25519
    {
        return Err(VerifyError::UnsupportedAlg {
            alg: alg.to_owned(),
        });
    }

    // The kid is a key-selection hint and is NOT covered by the signature.
    // That is safe: rewriting it can only select a key under which
    // verification fails, never make an invalid signature pass. The signing
    // time is the opposite case and IS covered — see `signed_material`.
    let (ring_kid, key, policy) = ring.get(kid).ok_or_else(|| VerifyError::UnknownKid {
        kid: kid.to_owned(),
    })?;

    let raw = base64_decode(sig_b64).map_err(VerifyError::Malformed)?;
    let sig = Signature::from_slice(&raw).map_err(|e| VerifyError::Malformed(e.to_string()))?;
    key.verify(&signed_material(at_ms, body), &sig)
        .map_err(|_| VerifyError::BadSignature {
            kid: kid.to_owned(),
        })?;

    // Freshness is checked only AFTER the signature holds, so a stale verdict
    // is always about a genuine message. Checking it first would let anyone
    // provoke a `Stale` report by sending garbage with an old timestamp.
    if let Some(max_age) = policy.max_age {
        let age_ms = now_ms - at_ms;
        let max_age_ms = max_age.as_millis();
        // Compared in i128 rather than casting the bound down to i64. A
        // `max_age_secs` large enough to overflow the cast is nonsense config
        // rather than an attack (the registry holding it is ACL'd), but the
        // truncation would silently turn a bound into its opposite — a
        // negative limit that nothing can satisfy, or one so large the key is
        // effectively unbounded — and `-(x as i64)` can panic outright on
        // i64::MIN in a debug build. i128 holds every value `Duration::as_millis`
        // can produce, so there is nothing left to get wrong.
        let age = age_ms as i128;
        let bound = max_age_ms as i128;
        // A signature from the future is not "fresh" — it is a clock that
        // disagrees, and treating it as valid would let a skewed or hostile
        // signer mint credentials that outlive the bound. Bound both ends.
        if age > bound || age < -bound {
            return Err(VerifyError::Stale {
                kid: kid.to_owned(),
                age_ms,
                max_age_ms,
            });
        }
    }

    Ok(Verified {
        kid: ring_kid,
        policy,
    })
}

/// Sign `body`, producing the headers to publish alongside it.
///
/// Lives here rather than in the backend so the signing and verifying halves
/// cannot drift — a round-trip test in this module covers both at once.
pub fn sign(key: &SigningKey, kid: &str, body: &[u8], at_ms: i64) -> SigHeaders {
    let sig = key.sign(&signed_material(at_ms, body));
    SigHeaders {
        sig_b64: Some(base64_encode(&sig.to_bytes())),
        kid: Some(kid.to_owned()),
        alg: Some(ALG_ED25519.to_owned()),
        at_ms: Some(at_ms.to_string()),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "now" so the tests never depend on wall-clock time.
    const NOW: i64 = 1_700_000_000_000;

    fn keypair(seed: u8) -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    fn ring_with(kid: &str, vk: VerifyingKey) -> KeyRing {
        let mut r = KeyRing::new();
        r.insert(kid, vk, KeyPolicy::backend("backend"));
        r
    }

    #[test]
    fn round_trip_accepts_the_bytes_that_were_signed() {
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        let body = br#"{"id":"job","request_id":"r1"}"#;

        let headers = sign(&sk, "backend-1", body, NOW);
        let ok = verify(&ring, body, &headers, NOW).expect("verifies");
        assert_eq!(ok.kid, "backend-1");
        assert_eq!(ok.policy.label, "backend");
    }

    #[test]
    fn a_single_flipped_byte_fails() {
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        let headers = sign(&sk, "backend-1", b"run this", NOW);
        // The whole point: bytes the backend did not author must not execute.
        assert_eq!(
            verify(&ring, b"run thit", &headers, NOW),
            Err(VerifyError::BadSignature {
                kid: "backend-1".into()
            })
        );
    }

    #[test]
    fn a_forged_command_from_another_key_fails() {
        // The #1155 attacker: holds a NATS credential, can place bytes on a
        // command subject, and signs with a key of their own.
        let (_, backend_vk) = keypair(1);
        let (attacker_sk, _) = keypair(9);
        let ring = ring_with("backend-1", backend_vk);

        let body = b"malicious";
        // Signed with the attacker's key but claiming the backend's kid — the
        // kid being outside the signature buys them nothing.
        let headers = sign(&attacker_sk, "backend-1", body, NOW);
        assert_eq!(
            verify(&ring, body, &headers, NOW),
            Err(VerifyError::BadSignature {
                kid: "backend-1".into()
            })
        );
    }

    #[test]
    fn unsigned_is_distinct_from_every_failure() {
        let (_, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        // Stages 1–2 deliver unsigned commands as a matter of course. If this
        // collapsed into the same error as a bad signature, a real forgery
        // would be indistinguishable from normal traffic for the whole
        // migration window.
        assert_eq!(
            verify(&ring, b"anything", &SigHeaders::default(), NOW),
            Err(VerifyError::Unsigned)
        );
    }

    #[test]
    fn an_unknown_kid_is_its_own_error() {
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        let headers = sign(&sk, "backend-2", b"body", NOW);
        // The mid-rotation state: correctly signed, but this agent has not
        // been given the new key. Reported distinctly because "your keyring is
        // stale" and "someone forged this" need opposite responses.
        assert_eq!(
            verify(&ring, b"body", &headers, NOW),
            Err(VerifyError::UnknownKid {
                kid: "backend-2".into()
            })
        );
    }

    #[test]
    fn rotation_is_two_kids_on_one_ring() {
        // Rotation shares its implementation with multi-signer rather than
        // being a separate mechanism — this is that claim, as a test.
        let (old_sk, old_vk) = keypair(1);
        let (new_sk, new_vk) = keypair(2);
        let mut ring = ring_with("backend-1", old_vk);
        ring.insert("backend-2", new_vk, KeyPolicy::backend("backend (new)"));

        for (sk, kid) in [(&old_sk, "backend-1"), (&new_sk, "backend-2")] {
            let headers = sign(sk, kid, b"during the window", NOW);
            assert_eq!(
                verify(&ring, b"during the window", &headers, NOW)
                    .unwrap()
                    .kid,
                kid
            );
        }
    }

    #[test]
    fn policies_are_per_key_not_per_ring() {
        // Without this, "multiple signers" means "more keys that can do
        // everything", which is worse than one key.
        let (_, backend_vk) = keypair(1);
        let (bg_sk, bg_vk) = keypair(3);
        let mut ring = ring_with("backend-1", backend_vk);
        ring.insert(
            "break-glass",
            bg_vk,
            KeyPolicy::break_glass("break-glass", Duration::from_secs(300)),
        );

        let headers = sign(&bg_sk, "break-glass", b"emergency", NOW);
        let ok = verify(&ring, b"emergency", &headers, NOW).unwrap();
        assert!(
            ok.policy.audit_every_use,
            "break-glass use must be recorded"
        );
        assert_eq!(ok.policy.max_age, Some(Duration::from_secs(300)));

        // The ordinary signer keeps the ordinary policy.
        let (sk, vk) = keypair(1);
        let mut r2 = KeyRing::new();
        r2.insert("backend-1", vk, KeyPolicy::backend("backend"));
        let h = sign(&sk, "backend-1", b"routine", NOW);
        assert!(
            !verify(&r2, b"routine", &h, NOW)
                .unwrap()
                .policy
                .audit_every_use
        );
    }

    #[test]
    fn partial_or_unreadable_headers_are_rejected_rather_than_ignored() {
        let (_, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);

        // A signature with no kid cannot be attributed. Treating it as
        // "unsigned" would let an attacker strip the kid to slip a message
        // through the stage-1/2 accept-unsigned path.
        let no_kid = SigHeaders {
            sig_b64: Some("AAAA".into()),
            kid: None,
            alg: None,
            at_ms: None,
        };
        assert!(matches!(
            verify(&ring, b"x", &no_kid, NOW),
            Err(VerifyError::Malformed(_))
        ));

        // A kid with no signature is equally not "unsigned".
        let no_sig = SigHeaders {
            sig_b64: None,
            kid: Some("backend-1".into()),
            alg: None,
            at_ms: None,
        };
        assert!(matches!(
            verify(&ring, b"x", &no_sig, NOW),
            Err(VerifyError::Malformed(_))
        ));

        // Only the algorithm header, with nothing to verify. It still claims
        // to be signed, so it must not fall through to the accept-unsigned
        // path: the rule is "no signing headers at all", not "none of the two
        // that carry the signature".
        let alg_only = SigHeaders {
            sig_b64: None,
            kid: None,
            alg: Some(ALG_ED25519.into()),
            at_ms: None,
        };
        assert!(matches!(
            verify(&ring, b"x", &alg_only, NOW),
            Err(VerifyError::Malformed(_))
        ));

        // Not base64 at all.
        let junk = SigHeaders {
            sig_b64: Some("!!!not base64!!!".into()),
            kid: Some("backend-1".into()),
            alg: None,
            at_ms: Some(NOW.to_string()),
        };
        assert!(matches!(
            verify(&ring, b"x", &junk, NOW),
            Err(VerifyError::Malformed(_))
        ));
    }

    #[test]
    fn an_unexpected_algorithm_is_refused_not_assumed() {
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        let mut headers = sign(&sk, "backend-1", b"body", NOW);
        headers.alg = Some("hmac-sha256".into());
        assert_eq!(
            verify(&ring, b"body", &headers, NOW),
            Err(VerifyError::UnsupportedAlg {
                alg: "hmac-sha256".into()
            })
        );
    }

    #[test]
    fn an_absent_alg_header_means_the_original_shape() {
        // Forward compatibility in the other direction: a signer that predates
        // the alg header must still verify.
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        let mut headers = sign(&sk, "backend-1", b"body", NOW);
        headers.alg = None;
        assert!(verify(&ring, b"body", &headers, NOW).is_ok());
    }

    #[test]
    fn the_signing_time_is_covered_by_the_signature() {
        // The asymmetry with `kid`: rewriting the timestamp must not verify,
        // or a captured message could be replayed by simply making it look
        // fresh. This is the whole reason `at` sits inside the signed
        // material.
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        let mut headers = sign(&sk, "backend-1", b"body", NOW);
        headers.at_ms = Some((NOW + 1).to_string());
        assert_eq!(
            verify(&ring, b"body", &headers, NOW),
            Err(VerifyError::BadSignature {
                kid: "backend-1".into()
            })
        );
    }

    #[test]
    fn the_ordinary_signer_accepts_a_week_old_command() {
        // Not a nicety: the agent's JetStream replay redelivers the retained
        // command per subject for 7 days, so an agent reconnecting after a
        // week legitimately receives week-old commands. A freshness bound on
        // the backend key would reject exactly that traffic.
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);
        let week = 7 * 24 * 60 * 60 * 1000;
        let headers = sign(&sk, "backend-1", b"scheduled job", NOW - week);
        assert!(verify(&ring, b"scheduled job", &headers, NOW).is_ok());
    }

    #[test]
    fn a_bounded_key_rejects_an_old_signature() {
        let (sk, vk) = keypair(3);
        let mut ring = KeyRing::new();
        ring.insert(
            "break-glass",
            vk,
            KeyPolicy::break_glass("break-glass", Duration::from_secs(300)),
        );

        // Inside the window.
        let fresh = sign(&sk, "break-glass", b"emergency", NOW - 60_000);
        assert!(verify(&ring, b"emergency", &fresh, NOW).is_ok());

        // Past it. This is the case that stops a first-boot agent executing a
        // week-old emergency command: its dedup cache is empty, so freshness
        // is the only thing left.
        let old = sign(&sk, "break-glass", b"emergency", NOW - 600_000);
        assert_eq!(
            verify(&ring, b"emergency", &old, NOW),
            Err(VerifyError::Stale {
                kid: "break-glass".into(),
                age_ms: 600_000,
                max_age_ms: 300_000,
            })
        );
    }

    #[test]
    fn a_signature_from_the_future_is_not_fresh() {
        // A clock that disagrees, or a signer trying to mint something that
        // outlives its bound. Bounding only the old side would let a
        // far-future timestamp stay valid indefinitely.
        let (sk, vk) = keypair(3);
        let mut ring = KeyRing::new();
        ring.insert(
            "break-glass",
            vk,
            KeyPolicy::break_glass("break-glass", Duration::from_secs(300)),
        );
        let ahead = sign(&sk, "break-glass", b"emergency", NOW + 3_600_000);
        assert!(matches!(
            verify(&ring, b"emergency", &ahead, NOW),
            Err(VerifyError::Stale { .. })
        ));
        // Modest skew inside the window still passes, so a machine a few
        // seconds ahead is not locked out.
        let skewed = sign(&sk, "break-glass", b"emergency", NOW + 5_000);
        assert!(verify(&ring, b"emergency", &skewed, NOW).is_ok());
    }

    #[test]
    fn freshness_is_only_judged_after_the_signature_holds() {
        // Otherwise anyone could provoke a `Stale` report — which at stage 3
        // is a rejection an operator has to investigate — by sending garbage
        // with an old timestamp.
        let (_, vk) = keypair(3);
        let mut ring = KeyRing::new();
        ring.insert(
            "break-glass",
            vk,
            KeyPolicy::break_glass("break-glass", Duration::from_secs(300)),
        );
        let forged = SigHeaders {
            sig_b64: Some(base64_encode(&[7u8; 64])),
            kid: Some("break-glass".into()),
            alg: Some(ALG_ED25519.into()),
            at_ms: Some((NOW - 600_000).to_string()),
        };
        assert_eq!(
            verify(&ring, b"x", &forged, NOW),
            Err(VerifyError::BadSignature {
                kid: "break-glass".into()
            })
        );
    }

    #[test]
    fn a_missing_or_unparseable_signing_time_is_malformed() {
        let (sk, vk) = keypair(1);
        let ring = ring_with("backend-1", vk);

        let mut no_at = sign(&sk, "backend-1", b"body", NOW);
        no_at.at_ms = None;
        assert!(matches!(
            verify(&ring, b"body", &no_at, NOW),
            Err(VerifyError::Malformed(_))
        ));

        let mut junk_at = sign(&sk, "backend-1", b"body", NOW);
        junk_at.at_ms = Some("yesterday".into());
        assert!(matches!(
            verify(&ring, b"body", &junk_at, NOW),
            Err(VerifyError::Malformed(_))
        ));

        // And a lone `at` header still counts as a signing claim rather than
        // an absent one.
        let at_only = SigHeaders {
            sig_b64: None,
            kid: None,
            alg: None,
            at_ms: Some(NOW.to_string()),
        };
        assert!(matches!(
            verify(&ring, b"body", &at_only, NOW),
            Err(VerifyError::Malformed(_))
        ));
    }

    #[test]
    fn an_absurd_bound_neither_panics_nor_inverts() {
        // `max_age_secs` reaches this from JSON with no upper bound. A value
        // this large is a typo rather than an attack, but the old `as i64`
        // cast would have truncated it into a negative limit that nothing can
        // satisfy — turning "almost never expires" into "always expired" —
        // and could panic on the negation in a debug build.
        let (sk, vk) = keypair(5);
        let mut ring = KeyRing::new();
        ring.insert(
            "silly",
            vk,
            KeyPolicy::break_glass("silly", Duration::from_secs(u64::MAX / 1000)),
        );
        let headers = sign(&sk, "silly", b"body", NOW - 10_000);
        assert!(
            verify(&ring, b"body", &headers, NOW).is_ok(),
            "an enormous bound must read as permissive, not as inverted"
        );
    }
}
