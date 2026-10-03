//! The versioned command envelope: a command that names its recipient and
//! carries an absolute expiry **inside the signed bytes**.
//!
//! A legacy command is the bare serialized [`Command`], signed over its exact
//! bytes. That signature is valid for any recipient and, for the backend key,
//! forever — so a captured command can be replayed to another host, or much
//! later. The envelope closes both holes by making the recipient and the
//! deadline part of what the signature covers.
//!
//! It is a type distinct from [`Command`] on purpose. `Command` is also built
//! locally (the scheduler, in-process callers) where a recipient and expiry
//! mean nothing, and an agent that predates this type fails to parse the
//! envelope body as a `Command` (required fields are missing) instead of
//! running it without the recipient and expiry checks.
//!
//! # Clock policy
//!
//! Every check here compares against the **host's wall clock**, so the whole
//! scheme assumes host time is trustworthy. Arbitrary clock rollback defeats
//! any wall-clock freshness bound: a host whose clock is set back accepts an
//! envelope that has really expired. That is inherent, not something this
//! module can repair; it narrows the replay window rather than closing it for
//! a host whose clock the attacker controls.
//!
//! The policy is deliberately conservative about *locking hosts out*, because
//! a rejected envelope is a command that silently does not run:
//!
//! * the future side tolerates [`FUTURE_SKEW_ALLOWANCE`] of disagreement
//!   between the signer's clock and the host's, and is otherwise unbounded —
//!   ordinary freshness is bounded only by `expires_at`;
//! * the validity window (`expires_at` minus signing time) may not exceed
//!   [`MAX_ENVELOPE_VALIDITY`], enforced by the agent independently of
//!   whatever the signer chose;
//! * clock-related refusals are distinct [`EnvelopeError`] values, so an
//!   operator can tell a skewed clock from tampering.
//!
//! A host whose clock runs more than the allowance *behind* the signer sees
//! every envelope as future-dated and refuses all of them: clock offset must
//! be observed before a host is switched to receive envelopes.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Command;
use crate::signing::{self, KeyRing, SigHeaders, Signer, VerifyError};

/// The `kind` discriminator of the v2 envelope.
pub const ENVELOPE_KIND_V2: &str = "kanade.command.v2";

/// Protocol name reported for the original, unwrapped signed `Command`.
pub const PROTOCOL_LEGACY: &str = "legacy";
/// Protocol name reported for the v2 envelope; equal to its `kind`.
pub const PROTOCOL_V2: &str = ENVELOPE_KIND_V2;

/// Longest validity window an agent accepts, measured from the signing time to
/// `expires_at`.
///
/// Matches the seven-day retention of the command stream: nothing legitimately
/// needs to be deliverable for longer, and the agent enforces it itself so a
/// signer that picks a larger expiry cannot widen the replay window. A
/// consequence worth knowing: an agent that was offline for more than this
/// refuses the stale replay of a command it missed, which is intended.
pub const MAX_ENVELOPE_VALIDITY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How far in the future a signing time may be before the host calls it a clock
/// disagreement. The same tolerance a break-glass key's window already needs.
pub const FUTURE_SKEW_ALLOWANCE: Duration = Duration::from_secs(60 * 60);

/// The command protocols this build can verify, as reported in the heartbeat.
pub fn supported_command_protocols() -> Vec<String> {
    vec![PROTOCOL_LEGACY.to_owned(), PROTOCOL_V2.to_owned()]
}

/// The network form of a v2 command.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    pub kind: String,
    /// The exact registered pc_id of the one host allowed to run this; case is
    /// preserved and compared byte for byte.
    pub target_pc_id: String,
    /// Absolute UTC deadline for **starting** the command. Not an instruction
    /// to kill a process that is already running.
    pub expires_at: DateTime<Utc>,
    pub command: Command,
}

/// Why an envelope was not accepted. Each variant is a distinct operational
/// state; the clock ones are kept apart from signature failures on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The signature did not verify (or there was none).
    Signature(VerifyError),
    /// A `kind` this build does not recognise.
    UnknownKind(String),
    /// The body is not a well-formed envelope.
    Malformed(String),
    /// No `target_pc_id`.
    MissingRecipient,
    /// Addressed to a different host.
    Misaddressed { target: String },
    /// No `expires_at`.
    MissingExpiry,
    /// `expires_at` is not an RFC 3339 timestamp.
    MalformedExpiry(String),
    /// Signed further in the future than the allowance: the clocks disagree.
    ClockAhead { ahead_ms: i128 },
    /// `expires_at` is before the signing time.
    Reversed,
    /// The validity window exceeds [`MAX_ENVELOPE_VALIDITY`].
    OverCeiling { validity_ms: i128 },
    /// The effective start deadline has passed.
    Expired { deadline: DateTime<Utc> },
    /// The signing key's own freshness bound has passed.
    KeyAgeExceeded {
        kid: String,
        age_ms: i128,
        max_age_ms: i128,
    },
}

impl EnvelopeError {
    /// Whether the refusal is about time rather than about who or what.
    pub fn is_clock(&self) -> bool {
        matches!(
            self,
            EnvelopeError::ClockAhead { .. }
                | EnvelopeError::Reversed
                | EnvelopeError::OverCeiling { .. }
                | EnvelopeError::Expired { .. }
                | EnvelopeError::KeyAgeExceeded { .. }
        )
    }
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeError::Signature(e) => write!(f, "{e}"),
            EnvelopeError::UnknownKind(k) => write!(f, "unrecognised command kind {k:?}"),
            EnvelopeError::Malformed(e) => write!(f, "malformed command envelope: {e}"),
            EnvelopeError::MissingRecipient => write!(f, "envelope names no recipient"),
            EnvelopeError::Misaddressed { target } => {
                write!(f, "envelope is addressed to {target:?}, not this host")
            }
            EnvelopeError::MissingExpiry => write!(f, "envelope carries no expiry"),
            EnvelopeError::MalformedExpiry(e) => write!(f, "envelope expiry is malformed: {e}"),
            EnvelopeError::ClockAhead { ahead_ms } => write!(
                f,
                "signed {ahead_ms}ms in the future, beyond the {}s clock allowance — the \
                 signer's and this host's clocks disagree",
                FUTURE_SKEW_ALLOWANCE.as_secs()
            ),
            EnvelopeError::Reversed => write!(f, "envelope expires before it was signed"),
            EnvelopeError::OverCeiling { validity_ms } => write!(
                f,
                "envelope validity of {validity_ms}ms exceeds the {}s ceiling",
                MAX_ENVELOPE_VALIDITY.as_secs()
            ),
            EnvelopeError::Expired { deadline } => {
                write!(f, "envelope start deadline {deadline} has passed")
            }
            EnvelopeError::KeyAgeExceeded {
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

impl std::error::Error for EnvelopeError {}

/// An envelope that passed every check.
#[derive(Debug, Clone)]
pub struct VerifiedEnvelope {
    pub command: Command,
    /// The earlier of `expires_at` and the command's own `deadline_at`. The
    /// latest moment the command may **start**.
    pub start_deadline: DateTime<Utc>,
    pub kid: String,
}

/// Whether a start deadline has passed. Inclusive: starting exactly at the
/// deadline is allowed, matching the existing `deadline_at` boundary.
pub fn start_deadline_passed(deadline: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now > deadline
}

/// Build and sign an envelope. Returns the exact bytes to publish and the
/// signature headers that cover them.
///
/// The bytes are serialized once and signed as-is; verification is over the
/// received bytes, never over a re-serialised value. Refuses a window that
/// the agent would refuse anyway, so a bad expiry fails at the signer rather
/// than as a fleet-wide refusal.
pub fn sign_envelope(
    signer: &Signer,
    target_pc_id: &str,
    expires_at: DateTime<Utc>,
    command: Command,
    now: DateTime<Utc>,
) -> Result<(Vec<u8>, SigHeaders), EnvelopeError> {
    if target_pc_id.is_empty() {
        return Err(EnvelopeError::MissingRecipient);
    }
    let validity = expires_at.timestamp_millis() as i128 - now.timestamp_millis() as i128;
    if validity < 0 {
        return Err(EnvelopeError::Reversed);
    }
    if validity > MAX_ENVELOPE_VALIDITY.as_millis() as i128 {
        return Err(EnvelopeError::OverCeiling {
            validity_ms: validity,
        });
    }
    let envelope = CommandEnvelope {
        kind: ENVELOPE_KIND_V2.to_owned(),
        target_pc_id: target_pc_id.to_owned(),
        expires_at,
        command,
    };
    let body =
        serde_json::to_vec(&envelope).map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
    let headers = signer.headers(&body, now.timestamp_millis());
    Ok((body, headers))
}

/// Verify a received envelope. Pure: the clock is passed in, so the admission
/// step and the tests can call it without a broker or a real clock.
///
/// Order matters. The signature is checked over the received bytes first, and
/// only then is the body parsed strictly, so forged bytes cannot provoke a
/// clock or recipient report. After that: kind, recipient, expiry presence,
/// then the time checks (future skew, reversed, ceiling, expiry, key age).
pub fn verify_envelope(
    ring: &KeyRing,
    body: &[u8],
    headers: &SigHeaders,
    my_pc_id: &str,
    now: DateTime<Utc>,
) -> Result<VerifiedEnvelope, EnvelopeError> {
    let auth = signing::verify_signature(ring, body, headers).map_err(EnvelopeError::Signature)?;

    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| EnvelopeError::Malformed("not a JSON object".into()))?;
    match obj.get("kind").and_then(|k| k.as_str()) {
        Some(ENVELOPE_KIND_V2) => {}
        Some(other) => return Err(EnvelopeError::UnknownKind(other.to_owned())),
        None => return Err(EnvelopeError::UnknownKind(format!("{:?}", obj.get("kind")))),
    }
    if let Some(unknown) = obj.keys().find(|k| {
        !matches!(
            k.as_str(),
            "kind" | "target_pc_id" | "expires_at" | "command"
        )
    }) {
        return Err(EnvelopeError::Malformed(format!(
            "unknown field {unknown:?}"
        )));
    }

    let target = match obj.get("target_pc_id") {
        None | Some(serde_json::Value::Null) => return Err(EnvelopeError::MissingRecipient),
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.as_str(),
        Some(_) => return Err(EnvelopeError::MissingRecipient),
    };
    if target != my_pc_id {
        return Err(EnvelopeError::Misaddressed {
            target: target.to_owned(),
        });
    }

    let expires_at = match obj.get("expires_at") {
        None | Some(serde_json::Value::Null) => return Err(EnvelopeError::MissingExpiry),
        Some(serde_json::Value::String(s)) => DateTime::parse_from_rfc3339(s)
            .map_err(|e| EnvelopeError::MalformedExpiry(e.to_string()))?
            .with_timezone(&Utc),
        Some(_) => {
            return Err(EnvelopeError::MalformedExpiry(
                "expires_at is not a string".into(),
            ));
        }
    };
    let command: Command = serde_json::from_value(
        obj.get("command")
            .cloned()
            .ok_or_else(|| EnvelopeError::Malformed("missing command".into()))?,
    )
    .map_err(|e| EnvelopeError::Malformed(e.to_string()))?;

    // Wide integers throughout: the signing time is attacker-influenced only
    // through a genuine signature, but the subtraction must not wrap either way.
    let now_ms = now.timestamp_millis() as i128;
    let at_ms = auth.at_ms as i128;
    let expires_ms = expires_at.timestamp_millis() as i128;

    let ahead_ms = at_ms - now_ms;
    if ahead_ms > FUTURE_SKEW_ALLOWANCE.as_millis() as i128 {
        return Err(EnvelopeError::ClockAhead { ahead_ms });
    }
    if expires_ms < at_ms {
        return Err(EnvelopeError::Reversed);
    }
    let validity_ms = expires_ms - at_ms;
    if validity_ms > MAX_ENVELOPE_VALIDITY.as_millis() as i128 {
        return Err(EnvelopeError::OverCeiling { validity_ms });
    }

    let start_deadline = command
        .deadline_at
        .map_or(expires_at, |d| d.min(expires_at));
    if start_deadline_passed(start_deadline, now) {
        return Err(EnvelopeError::Expired {
            deadline: start_deadline,
        });
    }

    if let Some(max_age) = auth.policy.max_age {
        let age_ms = now_ms - at_ms;
        let max_age_ms = max_age.as_millis() as i128;
        if age_ms > max_age_ms {
            return Err(EnvelopeError::KeyAgeExceeded {
                kid: auth.kid.to_owned(),
                age_ms,
                max_age_ms,
            });
        }
    }

    Ok(VerifiedEnvelope {
        command,
        start_deadline,
        kid: auth.kid.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing::{KeyPolicy, generate_keypair};
    use crate::wire::{RunAs, Shell, Staleness};
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn command() -> Command {
        Command {
            id: "echo".into(),
            version: "1.0.0".into(),
            request_id: "req-1".into(),
            exec_id: Some("dep-1".into()),
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

    fn signer() -> Signer {
        Signer::new(generate_keypair().unwrap(), "backend-1")
    }

    fn ring(signer: &Signer, policy: KeyPolicy) -> KeyRing {
        let mut ring = KeyRing::new();
        ring.insert(signer.kid(), signer.verifying_key(), policy);
        ring
    }

    fn signed(s: &Signer, expires: i64) -> (Vec<u8>, SigHeaders) {
        sign_envelope(s, "PC-A", t(expires), command(), t(0)).unwrap()
    }

    /// Re-sign arbitrary bytes, for cases the typed builder refuses to make.
    fn resign(s: &Signer, body: &str, at: i64) -> (Vec<u8>, SigHeaders) {
        let b = body.as_bytes().to_vec();
        let h = s.headers(&b, t(at).timestamp_millis());
        (b, h)
    }

    #[test]
    fn round_trip_verifies_and_carries_the_command() {
        let s = signer();
        let (body, h) = signed(&s, 3600);
        let v =
            verify_envelope(&ring(&s, KeyPolicy::backend("b")), &body, &h, "PC-A", t(10)).unwrap();
        assert_eq!(v.command.request_id, "req-1");
        assert_eq!(v.start_deadline, t(3600));
        assert_eq!(v.kid, "backend-1");
    }

    #[test]
    fn tampering_with_any_field_fails_the_signature() {
        let s = signer();
        let r = ring(&s, KeyPolicy::backend("b"));
        let (body, h) = signed(&s, 3600);
        let text = String::from_utf8(body).unwrap();
        let exp = t(3600).to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
        for (from, to) in [
            ("echo hi", "rm -rf x"),
            ("PC-A", "PC-B"),
            (exp.as_str(), "2099-01-01T00:00:00Z"),
            ("req-1", "req-2"),
        ] {
            assert!(text.contains(from), "fixture lacks {from}");
            let tampered = text.replace(from, to);
            let err = verify_envelope(&r, tampered.as_bytes(), &h, "PC-B", t(10)).unwrap_err();
            assert!(
                matches!(
                    err,
                    EnvelopeError::Signature(VerifyError::BadSignature { .. })
                ),
                "{from}: {err}"
            );
        }
    }

    #[test]
    fn an_envelope_for_one_host_is_refused_on_another() {
        let s = signer();
        let (body, h) = signed(&s, 3600);
        let r = ring(&s, KeyPolicy::backend("b"));
        let err = verify_envelope(&r, &body, &h, "PC-B", t(10)).unwrap_err();
        assert!(matches!(err, EnvelopeError::Misaddressed { .. }), "{err}");
        // Case is preserved, not folded.
        let err = verify_envelope(&r, &body, &h, "pc-a", t(10)).unwrap_err();
        assert!(matches!(err, EnvelopeError::Misaddressed { .. }), "{err}");
    }

    #[test]
    fn expired_is_refused_and_the_boundary_is_inclusive() {
        let s = signer();
        let (body, h) = signed(&s, 100);
        let r = ring(&s, KeyPolicy::backend("b"));
        assert!(verify_envelope(&r, &body, &h, "PC-A", t(100)).is_ok());
        let err = verify_envelope(&r, &body, &h, "PC-A", t(101)).unwrap_err();
        assert!(matches!(err, EnvelopeError::Expired { .. }), "{err}");
    }

    #[test]
    fn the_commands_own_deadline_wins_when_earlier() {
        let s = signer();
        let mut c = command();
        c.deadline_at = Some(t(50));
        let (body, h) = sign_envelope(&s, "PC-A", t(3600), c, t(0)).unwrap();
        let r = ring(&s, KeyPolicy::backend("b"));
        let v = verify_envelope(&r, &body, &h, "PC-A", t(10)).unwrap();
        assert_eq!(v.start_deadline, t(50));
        let err = verify_envelope(&r, &body, &h, "PC-A", t(60)).unwrap_err();
        assert!(matches!(err, EnvelopeError::Expired { deadline } if deadline == t(50)));
    }

    #[test]
    fn over_ceiling_is_refused_by_the_agent_even_if_the_signer_allowed_it() {
        let s = signer();
        let r = ring(&s, KeyPolicy::backend("b"));
        let far = MAX_ENVELOPE_VALIDITY.as_secs() as i64 + 1;
        // The builder refuses it ...
        assert!(matches!(
            sign_envelope(&s, "PC-A", t(far), command(), t(0)),
            Err(EnvelopeError::OverCeiling { .. })
        ));
        // ... and the verifier does not rely on that.
        let body = serde_json::to_string(&CommandEnvelope {
            kind: ENVELOPE_KIND_V2.into(),
            target_pc_id: "PC-A".into(),
            expires_at: t(far),
            command: command(),
        })
        .unwrap();
        let (b, h) = resign(&s, &body, 0);
        let err = verify_envelope(&r, &b, &h, "PC-A", t(10)).unwrap_err();
        assert!(matches!(err, EnvelopeError::OverCeiling { .. }), "{err}");
        // Exactly the ceiling is fine.
        let ok = body.replace(
            &t(far).to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            &t(far - 1).to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        );
        let (b, h) = resign(&s, &ok, 0);
        assert!(verify_envelope(&r, &b, &h, "PC-A", t(10)).is_ok());
    }

    #[test]
    fn future_dated_beyond_the_allowance_is_a_clock_refusal() {
        let s = signer();
        let r = ring(&s, KeyPolicy::backend("b"));
        let (body, h) = signed(&s, 7200);
        // Host clock 1h behind the signer: exactly at the allowance still ok.
        let at_edge = t(0) - chrono::Duration::seconds(FUTURE_SKEW_ALLOWANCE.as_secs() as i64);
        assert!(verify_envelope(&r, &body, &h, "PC-A", at_edge).is_ok());
        let behind = at_edge - chrono::Duration::seconds(1);
        let err = verify_envelope(&r, &body, &h, "PC-A", behind).unwrap_err();
        assert!(matches!(err, EnvelopeError::ClockAhead { .. }), "{err}");
        assert!(err.is_clock());
    }

    #[test]
    fn reversed_range_is_refused_distinctly() {
        let s = signer();
        let r = ring(&s, KeyPolicy::backend("b"));
        let body = serde_json::to_string(&CommandEnvelope {
            kind: ENVELOPE_KIND_V2.into(),
            target_pc_id: "PC-A".into(),
            expires_at: t(-100),
            command: command(),
        })
        .unwrap();
        let (b, h) = resign(&s, &body, 0);
        let err = verify_envelope(&r, &b, &h, "PC-A", t(-200)).unwrap_err();
        assert_eq!(err, EnvelopeError::Reversed);
    }

    #[test]
    fn a_break_glass_keys_own_shorter_bound_still_applies() {
        let s = signer();
        let r = ring(&s, KeyPolicy::break_glass("bg", Duration::from_secs(300)));
        let (body, h) = signed(&s, 3600);
        assert!(verify_envelope(&r, &body, &h, "PC-A", t(299)).is_ok());
        let err = verify_envelope(&r, &body, &h, "PC-A", t(301)).unwrap_err();
        assert!(matches!(err, EnvelopeError::KeyAgeExceeded { .. }), "{err}");
    }

    #[test]
    fn missing_or_malformed_fields_are_distinct_refusals() {
        let s = signer();
        let r = ring(&s, KeyPolicy::backend("b"));
        let cmd = serde_json::to_string(&command()).unwrap();
        let cases = [
            (
                format!(
                    r#"{{"kind":"kanade.command.v2","expires_at":"2027-01-01T00:00:00Z","command":{cmd}}}"#
                ),
                EnvelopeError::MissingRecipient,
            ),
            (
                format!(r#"{{"kind":"kanade.command.v2","target_pc_id":"PC-A","command":{cmd}}}"#),
                EnvelopeError::MissingExpiry,
            ),
        ];
        for (body, want) in cases {
            let (b, h) = resign(&s, &body, 0);
            assert_eq!(verify_envelope(&r, &b, &h, "PC-A", t(1)).unwrap_err(), want);
        }
        for bad in [r#""soon""#, "12345", "true"] {
            let body = format!(
                r#"{{"kind":"kanade.command.v2","target_pc_id":"PC-A","expires_at":{bad},"command":{cmd}}}"#
            );
            let (b, h) = resign(&s, &body, 0);
            let err = verify_envelope(&r, &b, &h, "PC-A", t(1)).unwrap_err();
            assert!(
                matches!(err, EnvelopeError::MalformedExpiry(_)),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn unknown_kind_and_unknown_fields_are_refused() {
        let s = signer();
        let r = ring(&s, KeyPolicy::backend("b"));
        let cmd = serde_json::to_string(&command()).unwrap();
        for kind in [r#""kanade.command.v3""#, "null", "7"] {
            let body = format!(
                r#"{{"kind":{kind},"target_pc_id":"PC-A","expires_at":"2027-01-01T00:00:00Z","command":{cmd}}}"#
            );
            let (b, h) = resign(&s, &body, 0);
            let err = verify_envelope(&r, &b, &h, "PC-A", t(1)).unwrap_err();
            assert!(
                matches!(err, EnvelopeError::UnknownKind(_)),
                "{kind}: {err}"
            );
        }
        let body = format!(
            r#"{{"kind":"kanade.command.v2","target_pc_id":"PC-A","expires_at":"2027-01-01T00:00:00Z","extra":1,"command":{cmd}}}"#
        );
        let (b, h) = resign(&s, &body, 0);
        assert!(matches!(
            verify_envelope(&r, &b, &h, "PC-A", t(1)).unwrap_err(),
            EnvelopeError::Malformed(_)
        ));
    }

    #[test]
    fn a_forged_envelope_cannot_provoke_a_clock_report() {
        let s = signer();
        let other = signer();
        let r = ring(&s, KeyPolicy::backend("b"));
        // Expired and future-dated, but signed by a key that is not on the ring.
        let (body, h) = sign_envelope(&other, "PC-A", t(10), command(), t(0)).unwrap();
        let err = verify_envelope(&r, &body, &h, "PC-A", t(1_000_000)).unwrap_err();
        assert!(matches!(err, EnvelopeError::Signature(_)), "{err}");
    }

    #[test]
    fn the_envelope_is_not_a_legacy_command() {
        let s = signer();
        let (body, _) = signed(&s, 3600);
        assert!(serde_json::from_slice::<Command>(&body).is_err());
    }

    #[test]
    fn the_supported_protocol_set_names_both() {
        assert_eq!(
            supported_command_protocols(),
            vec!["legacy".to_owned(), "kanade.command.v2".to_owned()]
        );
    }
}
