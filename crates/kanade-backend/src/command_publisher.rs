//! The one place a `Command` reaches the wire (#1165, rollout stage 2).
//!
//! Stage 1 (#1171) gave every agent the ability to verify a command's
//! provenance and to report what it saw without acting on it. This is stage 2:
//! the backend actually signs, so those reports flip from
//! `command_signature_absent` to `command_signature_ok` across the fleet.
//! Nothing is enforced yet — agents still accept unsigned commands — which is
//! what keeps the step reversible. Enforcement is stage 3.
//!
//! # Why a publisher type rather than a `sign()` call at each site
//!
//! There are four places the backend puts a `Command` on a command subject
//! (`api/exec.rs` × 3 for the immediate, delayed-wave and plain fan-out paths,
//! `api/run.rs` × 1 for ad-hoc runs), and the scheduler reaches the wire
//! through `exec_manifest` rather than publishing its own. A site that forgets
//! to sign does not fail: it publishes a command that works perfectly well
//! today and becomes an outage at stage 3, on whichever path is least
//! exercised. So the signing is not something a call site does — it is what
//! this type *is*, and the call sites cannot express "publish a command"
//! without it.
//!
//! # Signed at publish, not at build
//!
//! The signing time is covered by the signature (`signed_material`), and a
//! rollout wave can sit in a `tokio::spawn` for hours before it goes out.
//! Signing inside `publish` means the timestamp says when the bytes were sent
//! rather than when they were assembled — the thing a freshness bound is
//! trying to reason about. The backend key carries no bound (`KeyPolicy::
//! backend`), so this changes nothing today; it matters the moment a second,
//! bounded signer exists, and getting it wrong then would look like a clock
//! problem rather than a design one.
//!
//! # Not signing is a state, and it has two very different causes
//!
//! * **Nothing configured** — expected on any host where the key has not been
//!   minted yet, and during stage 2 it is simply "this backend has not been
//!   switched on". Warned once at startup.
//! * **Configured but unusable** — a half-written pair, an undecodable
//!   secret. Logged at `error`, because it is silent otherwise: the backend
//!   serves every request normally and the fleet quietly stays unsigned, which
//!   is indistinguishable from the first case unless someone says so.
//!
//! Both degrade to publishing unsigned rather than refusing to start. That is
//! correct *for stage 2* — a rollback lands on "no worse than today" instead
//! of an outage — and it stops being correct at stage 3, where a backend that
//! cannot sign can no longer command anything and should refuse to boot rather
//! than take the fleet down one command at a time.

use async_nats::HeaderMap;
use async_nats::client::PublishError;
use bytes::Bytes;
use kanade_shared::signing::{self, Signer};
use tracing::{error, info, warn};

/// Environment counterparts of the registry values, for backends on hosts
/// without a registry (the ARM64 Linux bundle keeps its secrets in
/// `/etc/kanade/kanade.env`, mode 0600). Same registry-then-env order the JWT
/// secret and static token already use in `auth.rs`.
const ENV_SIGNING_KEY: &str = "KANADE_COMMAND_SIGNING_KEY";
const ENV_SIGNING_KID: &str = "KANADE_COMMAND_SIGNING_KID";

/// Publishes commands, signing them when this backend holds a key.
pub struct CommandPublisher {
    nats: async_nats::Client,
    signer: Option<Signer>,
}

impl CommandPublisher {
    pub fn new(nats: async_nats::Client, signer: Option<Signer>) -> Self {
        Self { nats, signer }
    }

    /// Resolve the signing key from this host's secret store, reporting what
    /// it found. Never fails — see the module doc on why not signing is a
    /// degraded state rather than a fatal one at this stage.
    pub fn from_host(nats: async_nats::Client) -> Self {
        let signer = match resolve_signer() {
            Ok(Some(s)) => {
                info!(
                    kid = s.kid(),
                    identity = %identity_of(&s),
                    "signing every published command — agents holding other keys but not this one \
                     report command_signature_unknown_key; agents holding none report \
                     command_signature_unprovisioned. `identity` is what a correctly provisioned \
                     agent reports in command_keys (#1229) — an agent listing this kid with a \
                     different fingerprint holds the WRONG key: it reports \
                     command_signature_invalid for commands signed here, and once enforcement is \
                     on it refuses them."
                );
                Some(s)
            }
            Ok(None) => {
                warn!(
                    "no command-signing key on this host — commands are published unsigned. Run \
                     `kanade-backend command-key-generate` and distribute the public key to the \
                     fleet first (#1165)."
                );
                None
            }
            Err(e) => {
                error!(
                    error = %e,
                    "command-signing key is configured but unusable — commands are published \
                     UNSIGNED. This is not a warning about a missing key: something is set and \
                     wrong."
                );
                None
            }
        };
        Self::new(nats, signer)
    }

    /// The `kid` this backend signs with, or `None` when it is not signing.
    pub fn kid(&self) -> Option<&str> {
        self.signer.as_ref().map(Signer::kid)
    }

    /// This backend's own `kid:fingerprint` — the exact string a correctly
    /// provisioned agent reports in `command_keys` — or `None` when it is not
    /// signing.
    ///
    /// Exists so the fleet-wide question can be *compared* rather than assumed
    /// (#1229): the backend holds the public half already, so "does the fleet
    /// trust the key I actually sign with" needs no separate source of truth.
    pub fn identity(&self) -> Option<String> {
        self.signer.as_ref().map(identity_of)
    }

    /// The same identity split into its halves, for callers that need to
    /// compare rather than print — `GET /api/command-signing` (#1260).
    ///
    /// Both come from the live [`Signer`], so an API answer and the startup log
    /// line cannot disagree: there is one key, and both read it.
    pub fn identity_parts(&self) -> Option<(&str, String)> {
        self.signer.as_ref().map(identity_parts_of)
    }

    /// This backend's own public key as a `CommandKeys` keyring entry, or
    /// `None` when it is not signing.
    ///
    /// `GET /api/agents/installer` bakes this (and only this — never a
    /// break-glass key) into the generated install script so a freshly
    /// installed agent's ring trusts this backend from first boot and
    /// stage-3 enforcement works without a separate provisioning step.
    pub fn keyring_entry(&self) -> Option<serde_json::Value> {
        keyring_entry_of(self.signer.as_ref())
    }

    /// Publish a serialized `Command` to `subject`, signed if possible.
    pub async fn publish(&self, subject: String, payload: Bytes) -> Result<(), PublishError> {
        match headers_for(
            self.signer.as_ref(),
            &payload,
            chrono::Utc::now().timestamp_millis(),
        ) {
            Some(headers) => {
                self.nats
                    .publish_with_headers(subject, headers, payload)
                    .await
            }
            None => self.nats.publish(subject, payload).await,
        }
    }
}

/// The `kid:fingerprint` form, matching what an agent puts in `command_keys`.
/// One place, so the two sides cannot drift into formats that look comparable
/// and are not.
fn identity_of(signer: &Signer) -> String {
    let (kid, fp) = identity_parts_of(signer);
    format!("{kid}:{fp}")
}

/// The halves, for `GET /api/command-signing` (#1260). [`identity_of`] is
/// written in terms of this so the string an operator reads in a log and the
/// pair a caller compares against can never describe different keys.
fn identity_parts_of(signer: &Signer) -> (&str, String) {
    (signer.kid(), signing::fingerprint(&signer.verifying_key()))
}

/// The keyring entry a signer maps to, or `None` for a non-signing backend.
/// Split out from [`CommandPublisher::keyring_entry`] for the same reason
/// [`identity_parts_of`] is split out: the shape decision is reachable in a
/// unit test without a broker (a `CommandPublisher` needs a live
/// `async_nats::Client`).
fn keyring_entry_of(signer: Option<&Signer>) -> Option<serde_json::Value> {
    signer.map(|s| signing::keyring_entry(s.kid(), &s.verifying_key(), "backend"))
}

/// The headers to publish alongside `body`, or `None` when this backend is not
/// signing.
///
/// Split out from [`CommandPublisher::publish`] so the branch that decides
/// *whether a message claims to be signed* is reachable without a broker. The
/// unsigned case is the one worth pinning: it must attach **no** signature
/// header at all, because `SigHeaders::is_absent` treats any single one as a
/// signing claim — so a stray header would reclassify ordinary stage-2 traffic
/// from `Unsigned` (expected, silent) to `Malformed` (reported as a problem)
/// on every agent in the fleet at once.
fn headers_for(signer: Option<&Signer>, body: &[u8], at_ms: i64) -> Option<HeaderMap> {
    let h = signer?.headers(body, at_ms);
    let mut map = HeaderMap::new();
    for (name, value) in [
        (signing::SIG, h.sig_b64.as_deref()),
        (signing::SIG_KID, h.kid.as_deref()),
        (signing::SIG_ALG, h.alg.as_deref()),
        (signing::SIG_AT, h.at_ms.as_deref()),
    ] {
        if let Some(v) = value {
            map.insert(name, v);
        }
    }
    Some(map)
}

/// Which store a candidate key came from, so a half-configured host names the
/// one that needs fixing rather than the one it doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Registry,
    Env,
}

impl Source {
    fn describe(self) -> String {
        match self {
            Source::Registry => format!(
                "HKLM\\{}\\{{{}, {}}}",
                signing::REG_BACKEND_SUBKEY,
                signing::REG_SIGNING_KEY,
                signing::REG_SIGNING_KID
            ),
            Source::Env => format!("${ENV_SIGNING_KEY} / ${ENV_SIGNING_KID}"),
        }
    }
}

fn resolve_signer() -> Result<Option<Signer>, String> {
    let registry = (
        kanade_shared::secrets::read_hklm_value(
            signing::REG_BACKEND_SUBKEY,
            signing::REG_SIGNING_KEY,
        ),
        kanade_shared::secrets::read_hklm_value(
            signing::REG_BACKEND_SUBKEY,
            signing::REG_SIGNING_KID,
        ),
    );
    let env = (read_env(ENV_SIGNING_KEY), read_env(ENV_SIGNING_KID));
    match choose(registry, env)? {
        Some((secret, kid, source)) => Signer::from_secret(&secret, &kid)
            .map(Some)
            .map_err(|e| format!("{} in {}", e, source.describe())),
        None => Ok(None),
    }
}

fn read_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Pick the (secret, kid) pair, refusing to mix stores.
///
/// Pure, and separate from the registry and the environment, because
/// `read_hklm_value` returns `None` on non-Windows — a test that reached
/// through it would assert nothing at all on CI, which is exactly where these
/// guards need to hold. Same split as `resolve_kid` on the generating side.
///
/// The rule that matters is the one that looks like over-strictness: a store
/// holding *half* a pair is an error, not a reason to try the other store. A
/// registry key silently paired with an environment `kid` would sign real
/// commands under an id whose public half every agent holds for a different
/// key — reported fleet-wide as `command_signature_invalid`, which is the
/// forgery alarm, raised by a configuration mistake.
fn choose(
    registry: (Option<String>, Option<String>),
    env: (Option<String>, Option<String>),
) -> Result<Option<(String, String, Source)>, String> {
    for (source, (secret, kid)) in [(Source::Registry, registry), (Source::Env, env)] {
        // The "half a pair is an error" rule itself lives in `signing::pair`,
        // shared with the CLI's break-glass path. Only the phrasing is local —
        // naming the store an operator has to go fix is the whole value of
        // having two callers rather than one.
        let classified = signing::pair(secret.as_deref(), kid.as_deref())
            .map(|pair| pair.map(|(s, k)| (s.to_owned(), k.to_owned())));
        match classified {
            Ok(Some((secret, kid))) => {
                return Ok(Some((secret, kid, source)));
            }
            Ok(None) => continue,
            Err(signing::MissingHalf::Kid) => {
                return Err(format!(
                    "a command-signing key is present in {} but its key id is missing. The two \
                     are only meaningful together — signing under the wrong id is reported by \
                     every agent as an invalid signature.",
                    source.describe()
                ));
            }
            Err(signing::MissingHalf::Key) => {
                return Err(format!(
                    "a command-signing key id ({}) is present in {} but the key itself is missing.",
                    kid.as_deref().unwrap_or_default(),
                    source.describe()
                ));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::signing::{KeyPolicy, KeyRing, SigHeaders, VerifyError, verify};

    fn secret() -> String {
        signing::encode_secret(&signing::generate_keypair().unwrap())
    }

    #[test]
    fn the_reported_halves_rejoin_into_the_logged_identity() {
        // #1260: the API answers with the pair, the startup log prints the
        // string, and an operator compares one against the other. If they were
        // built independently they could disagree in a way that looks like a
        // fleet-wide key mismatch — so both go through `identity_parts_of`,
        // and this pins that they still do.
        let signer = Signer::from_secret(&secret(), "backend-1").unwrap();
        let (kid, fp) = identity_parts_of(&signer);
        assert_eq!(format!("{kid}:{fp}"), identity_of(&signer));
        assert_eq!(kid, "backend-1");
        // And the fingerprint is of the PUBLIC half, which is what an agent
        // reports — not of the secret, which nothing else could ever match.
        assert_eq!(fp, signing::fingerprint(&signer.verifying_key()));
    }

    #[test]
    fn a_complete_pair_is_taken_from_whichever_store_has_it() {
        let s = secret();
        let (got, kid, src) = choose((Some(s.clone()), Some("backend-1".into())), (None, None))
            .unwrap()
            .unwrap();
        assert_eq!(
            (got, kid, src),
            (s.clone(), "backend-1".to_string(), Source::Registry)
        );

        let (_, kid, src) = choose((None, None), (Some(s), Some("backend-2".into())))
            .unwrap()
            .unwrap();
        assert_eq!((kid, src), ("backend-2".to_string(), Source::Env));
    }

    #[test]
    fn the_registry_wins_when_both_stores_are_complete() {
        // Same precedence as the JWT secret and the static token in `auth.rs`.
        // Worth pinning: an operator debugging a Windows host by exporting the
        // env vars would otherwise silently change which key signs.
        let (_, kid, src) = choose(
            (Some(secret()), Some("from-registry".into())),
            (Some(secret()), Some("from-env".into())),
        )
        .unwrap()
        .unwrap();
        assert_eq!((kid.as_str(), src), ("from-registry", Source::Registry));
    }

    #[test]
    fn half_a_pair_is_an_error_rather_than_a_fallback() {
        // The case this whole function exists for. Falling through to the
        // other store would pair a key with a stranger's id and produce
        // `command_signature_invalid` on every machine — the forgery alarm,
        // raised by a typo.
        let err = choose(
            (Some(secret()), None),
            (Some(secret()), Some("from-env".into())),
        )
        .unwrap_err();
        assert!(err.contains("CommandSigningKid"), "{err}");

        let err = choose((None, Some("orphan".into())), (None, None)).unwrap_err();
        assert!(err.contains("orphan"), "{err}");
    }

    #[test]
    fn nothing_configured_is_not_an_error() {
        // Every host is in this state until the key is minted, and during
        // stage 2 it simply means "not switched on here yet".
        assert!(choose((None, None), (None, None)).unwrap().is_none());
    }

    #[test]
    fn the_headers_a_publisher_attaches_are_the_ones_an_agent_verifies() {
        // The cross-crate property stage 3 rests on: bytes signed here verify
        // against the ring an agent builds from the distributed public key.
        // Covered without a broker by going straight to the header map.
        let key = signing::generate_keypair().unwrap();
        let signer = Signer::from_secret(&signing::encode_secret(&key), "backend-1").unwrap();
        let body = br#"{"id":"job","request_id":"r1"}"#;
        let at = 1_700_000_000_000;
        let map = headers_for(Some(&signer), body, at).expect("a signing backend attaches headers");

        let headers = SigHeaders {
            sig_b64: map.get(signing::SIG).map(|v| v.to_string()),
            kid: map.get(signing::SIG_KID).map(|v| v.to_string()),
            alg: map.get(signing::SIG_ALG).map(|v| v.to_string()),
            at_ms: map.get(signing::SIG_AT).map(|v| v.to_string()),
        };
        let mut ring = KeyRing::new();
        ring.insert(
            "backend-1",
            key.verifying_key(),
            KeyPolicy::backend("backend"),
        );
        assert_eq!(verify(&ring, body, &headers, at).unwrap().kid, "backend-1");

        // All four travel, and none is empty: a partial set is classified
        // `Malformed` by the agent, not `Unsigned`.
        for name in [
            signing::SIG,
            signing::SIG_KID,
            signing::SIG_ALG,
            signing::SIG_AT,
        ] {
            let v = map.get(name).unwrap_or_else(|| panic!("{name} missing"));
            assert!(!v.to_string().is_empty(), "{name} is empty");
        }
    }

    #[test]
    fn a_non_signing_backend_attaches_nothing_at_all() {
        // The stage-2 fallback has to read as *absent* on the agent, not as a
        // broken signature. One stray header would be a signing claim nobody
        // can check — reported fleet-wide as a problem, on traffic that is
        // entirely normal until stage 3.
        assert!(headers_for(None, b"body", 0).is_none());
        assert_eq!(
            verify(&KeyRing::new(), b"body", &SigHeaders::default(), 0),
            Err(VerifyError::Unsigned)
        );
    }

    #[test]
    fn a_non_signing_backend_has_no_keyring_entry() {
        // The installer omits -CommandKeys entirely rather than shipping an
        // empty ring — an empty ring would parse as "provisioned, trusts
        // nobody" on the agent.
        assert!(keyring_entry_of(None).is_none());
    }

    #[test]
    fn the_keyring_entry_is_this_backends_own_public_key() {
        // GET /api/agents/installer bakes this into a fresh agent's ring, so
        // it must name the key the backend actually signs with — a kid or
        // public half assembled from anything else is the wrong-key ring that
        // refuses every command once enforcement is on.
        let key = signing::generate_keypair().unwrap();
        let signer = Signer::from_secret(&signing::encode_secret(&key), "backend-1").unwrap();
        let entry = keyring_entry_of(Some(&signer)).expect("a signing backend has an entry");
        assert_eq!(entry["kid"], signer.kid());
        assert_eq!(
            entry["public_key"],
            signing::encode_public(&signer.verifying_key())
        );
        assert_eq!(entry["label"], "backend");
        // And never anything but the public half: no field may carry the
        // secret into a file an operator ships around the fleet.
        let secret = signing::encode_secret(&key);
        assert!(
            !entry.to_string().contains(&secret),
            "keyring entry must not contain the signing secret"
        );
    }
}
