//! Signed package manifest contract (`package.manifest.json`, SPEC §2.13.4.3,
//! stage 2a): a pure, offline Ed25519 signature + integrity contract.
//!
//! What a successful [`verify_offline`] proves: the manifest bytes were signed
//! by the private half of a public key the *caller* obtained independently, the
//! signed identity is the one the caller expected, and the artifact's SHA-256 /
//! byte count equal the signed values.
//!
//! What it does **not** prove: that the key is currently authorised to
//! publish, that the manifest is fresh, or that the key is not revoked. Managed
//! trust distribution, rotation, revocation, freshness and agent-side
//! verification are stage 2b (`publisher_trust`, `package_admission`).
//! [`OfflineVerified`] therefore deliberately has no
//! conversion into any "deployable" or "trusted" state.
//!
//! # Key separation
//!
//! Package-publisher keys are a different authority from command-signing keys
//! (`signing`), even though both are Ed25519. Nothing here uses `signing`'s
//! `Signer` / `KeyRing`; signatures are domain separated ([`SIGNING_DOMAIN`]),
//! and trust comes only from a [`TrustedPublisherKey`] built from independent
//! input, never from the manifest. Domain separation cannot detect an operator
//! who reuses one secret for both roles: keeping the keys distinct is an
//! operational responsibility.
//!
//! # Signing bytes (v1)
//!
//! `SIGNING_DOMAIN` (ASCII, ending in NUL) followed by the compact UTF-8 JSON
//! of the manifest *without* `publisher.signature`, fields in the fixed order
//! of [`SigningView`]. The signed JSON includes `publisher.algorithm` and
//! `publisher.key_id`, so neither can be swapped after signing. The file form
//! is the same compact JSON including the signature, optionally followed by one
//! `\n`; any other spelling (whitespace, order, escapes) is rejected so a
//! single byte string corresponds to each manifest. Changing any of this
//! requires a new `manifest_version`.

use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::artifact::{ArtifactIdentity, ArtifactIntegrity, ArtifactSha256};

pub const MANIFEST_VERSION: &str = "1.0";
pub const ALGORITHM_ED25519: &str = "ed25519";
/// Prefix of every byte string signed or verified by this module.
pub const SIGNING_DOMAIN: &[u8] = b"kanade/package-manifest/ed25519/v1\0";
/// `purpose` required of a trust-input file.
pub const TRUST_PURPOSE: &str = "kanade-package-publisher-key-v1";
const KEY_ID_DOMAIN: &[u8] = b"kanade/package-publisher-key-id/v1\0";
const KEY_ID_PREFIX: &str = "ed25519:";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("malformed manifest: {0}")]
    Malformed(String),
    #[error("unsupported manifest_version {0:?} (supported: \"1.0\")")]
    UnsupportedVersion(String),
    #[error("unsupported signature algorithm {0:?} (supported: \"ed25519\")")]
    UnsupportedAlgorithm(String),
    #[error("manifest is not in canonical v1 form (compact JSON, fixed field order)")]
    NonCanonical,
    #[error("invalid manifest field: {0}")]
    Invalid(String),
    #[error("invalid trust input: {0}")]
    Trust(String),
    #[error("manifest key_id does not match the independently supplied trusted key")]
    KeyIdMismatch,
    #[error("signature verification failed")]
    SignatureInvalid,
    #[error("manifest identity differs from the expected identity")]
    IdentityMismatch,
    #[error("artifact size differs from the signed size_bytes")]
    SizeMismatch,
    #[error("artifact SHA-256 differs from the signed sha256")]
    DigestMismatch,
}

fn invalid(m: impl Into<String>) -> ManifestError {
    ManifestError::Invalid(m.into())
}

// ---------------------------------------------------------------- claims

/// Everything the signature covers besides the signer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestClaims {
    identity: ArtifactIdentity,
    package_id: String,
    min_os_version: String,
    required_agent_capabilities: Vec<String>,
    integrity: ArtifactIntegrity,
}

impl ManifestClaims {
    /// `min_os_version` は必須の非 null 文字列。capabilities は厳密昇順。
    pub fn new(
        identity: ArtifactIdentity,
        package_id: String,
        min_os_version: String,
        required_agent_capabilities: Vec<String>,
        integrity: ArtifactIntegrity,
    ) -> Result<Self, ManifestError> {
        validate_package_id(&package_id)?;
        validate_min_os_version(&min_os_version)?;
        validate_capabilities(&required_agent_capabilities)?;
        if integrity.size_bytes == 0 {
            return Err(invalid("size_bytes must be greater than zero"));
        }
        Ok(Self {
            identity,
            package_id,
            min_os_version,
            required_agent_capabilities,
            integrity,
        })
    }
    pub fn identity(&self) -> &ArtifactIdentity {
        &self.identity
    }
    pub fn package_id(&self) -> &str {
        &self.package_id
    }
    pub fn min_os_version(&self) -> &str {
        &self.min_os_version
    }
    pub fn required_agent_capabilities(&self) -> &[String] {
        &self.required_agent_capabilities
    }
    pub fn integrity(&self) -> &ArtifactIntegrity {
        &self.integrity
    }
}

/// 1..=128 bytes of `[a-z0-9._-]`, starting with `[a-z0-9]`.
pub(crate) fn validate_package_id(s: &str) -> Result<(), ManifestError> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        });
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "package_id must be 1-128 bytes of [a-z0-9._-] starting with [a-z0-9]",
        ))
    }
}

/// 1-4 dot-separated groups of 1-9 ASCII digits (e.g. `14.0`, `24.04`).
pub(crate) fn validate_min_os_version(s: &str) -> Result<(), ManifestError> {
    let parts: Vec<&str> = s.split('.').collect();
    let ok = (1..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| (1..=9).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "min_os_version must be 1-4 dot-separated groups of 1-9 digits",
        ))
    }
}

/// At most 32 entries of 1..=64 bytes `[a-z0-9_]`, strictly ascending
/// (so sorted and duplicate-free; ordering is part of the signed bytes).
fn validate_capabilities(caps: &[String]) -> Result<(), ManifestError> {
    if caps.len() > 32 {
        return Err(invalid("at most 32 required_agent_capabilities"));
    }
    for c in caps {
        let ok = !c.is_empty()
            && c.len() <= 64
            && c.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            return Err(invalid("capability must be 1-64 bytes of [a-z0-9_]"));
        }
    }
    if !caps.windows(2).all(|w| w[0] < w[1]) {
        return Err(invalid(
            "required_agent_capabilities must be strictly ascending (sorted, no duplicates)",
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ keys

/// Identity of a publisher public key: `ed25519:` + lowercase hex of the full
/// SHA-256 over a domain prefix and the 32 raw public-key bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherKeyId(String);

impl PublisherKeyId {
    pub fn of(key: &VerifyingKey) -> Self {
        let mut h = Sha256::new();
        h.update(KEY_ID_DOMAIN);
        h.update(key.as_bytes());
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Self(format!("{KEY_ID_PREFIX}{hex}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A publisher public key the caller obtained *independently* of the package
/// and manifest. There is deliberately no constructor from a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPublisherKey {
    key: VerifyingKey,
    key_id: PublisherKeyId,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustRecord {
    purpose: String,
    algorithm: String,
    key_id: String,
    public_key: String,
}

impl TrustedPublisherKey {
    pub fn from_verifying_key(key: VerifyingKey) -> Result<Self, ManifestError> {
        if key.is_weak() {
            return Err(ManifestError::Trust("weak publisher public key".into()));
        }
        let key_id = PublisherKeyId::of(&key);
        Ok(Self { key, key_id })
    }
    pub fn key_id(&self) -> &PublisherKeyId {
        &self.key_id
    }

    pub(crate) fn public_key_b64(&self) -> String {
        STANDARD.encode(self.key.as_bytes())
    }

    /// Canonical padded base64 of a 32-byte, non-weak, valid public key.
    pub(crate) fn from_public_key_b64(s: &str) -> Result<Self, ManifestError> {
        let t = |m: &str| ManifestError::Trust(m.to_owned());
        let raw = decode_b64_exact::<32>(s).ok_or_else(|| t("public_key"))?;
        let key =
            VerifyingKey::from_bytes(&raw).map_err(|_| t("public_key is not a valid point"))?;
        Self::from_verifying_key(key)
    }

    /// Strict Ed25519 verification of `msg` (callers add their own domain).
    pub(crate) fn verify_strict_bytes(&self, msg: &[u8], sig: &[u8; 64]) -> bool {
        self.key
            .verify_strict(msg, &Signature::from_bytes(sig))
            .is_ok()
    }

    /// Strict parse of the trust-input file: exact purpose, algorithm, a
    /// canonical base64 32-byte public key, and a `key_id` that must equal the
    /// one derived from that key.
    pub fn from_trust_json(bytes: &[u8]) -> Result<Self, ManifestError> {
        Self::from_record_json(bytes, TRUST_PURPOSE)
    }

    /// Same strict parse for any key record with an exact, caller-chosen
    /// `purpose` (the trust root of the managed publisher set reuses it, so a
    /// record minted for one role never parses as another).
    pub(crate) fn from_record_json(bytes: &[u8], purpose: &str) -> Result<Self, ManifestError> {
        let t = |m: &str| ManifestError::Trust(m.to_owned());
        let r: TrustRecord = serde_json::from_slice(bytes)
            .map_err(|e| ManifestError::Trust(format!("not a valid trust record: {e}")))?;
        if r.purpose != purpose {
            return Err(t("record purpose does not match the expected key role"));
        }
        if r.algorithm != ALGORITHM_ED25519 {
            return Err(ManifestError::UnsupportedAlgorithm(r.algorithm));
        }
        let raw = decode_b64_exact::<32>(&r.public_key).ok_or_else(|| t("public_key"))?;
        let key =
            VerifyingKey::from_bytes(&raw).map_err(|_| t("public_key is not a valid point"))?;
        let me = Self::from_verifying_key(key)?;
        if me.key_id.0 != r.key_id {
            return Err(t("key_id does not match public_key"));
        }
        Ok(me)
    }

    /// The trust-input file for this key (public material only).
    pub fn to_trust_json(&self) -> Vec<u8> {
        self.to_record_json(TRUST_PURPOSE)
    }

    pub(crate) fn to_record_json(&self, purpose: &str) -> Vec<u8> {
        let rec = TrustRecord {
            purpose: purpose.to_owned(),
            algorithm: ALGORITHM_ED25519.to_owned(),
            key_id: self.key_id.0.clone(),
            public_key: STANDARD.encode(self.key.as_bytes()),
        };
        let mut out = serde_json::to_vec(&rec).expect("trust record serializes");
        out.push(b'\n');
        out
    }
}

/// Package-publisher private key. Not interchangeable with command-signing
/// keys; the secret never appears in `Debug`, errors or any output here.
pub struct PublisherSigningKey(SigningKey);

impl std::fmt::Debug for PublisherSigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublisherSigningKey(<redacted>)")
    }
}

impl PublisherSigningKey {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }
    /// Standard padded base64 of the 32-byte Ed25519 seed. The error never
    /// echoes the input.
    pub fn from_seed_base64(raw: &str) -> Result<Self, ManifestError> {
        decode_b64_exact::<32>(raw.trim_end_matches(['\r', '\n']))
            .map(Self::from_seed)
            .ok_or_else(|| {
                ManifestError::Trust(
                    "private key must be padded base64 of exactly 32 bytes".to_owned(),
                )
            })
    }
    /// Raw signature over caller-domain-separated bytes.
    pub(crate) fn sign_bytes(&self, msg: &[u8]) -> [u8; 64] {
        self.0.sign(msg).to_bytes()
    }
    pub fn trusted_key(&self) -> TrustedPublisherKey {
        TrustedPublisherKey::from_verifying_key(self.0.verifying_key())
            .expect("a signing key derives a non-weak public key")
    }
}

/// Decode canonical padded base64 into exactly `N` bytes.
pub(crate) fn decode_b64_exact<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes = STANDARD.decode(s).ok()?;
    if STANDARD.encode(&bytes) != s {
        return None;
    }
    bytes.try_into().ok()
}

// ------------------------------------------------------------------ wire

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireManifest {
    manifest_version: String,
    identity: ArtifactIdentity,
    package_id: String,
    min_os_version: String,
    required_agent_capabilities: Vec<String>,
    sha256: ArtifactSha256,
    size_bytes: u64,
    publisher: WirePublisher,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePublisher {
    algorithm: String,
    key_id: String,
    signature: String,
}

/// Same field order as [`WireManifest`], minus `publisher.signature`.
#[derive(Serialize)]
struct SigningView<'a> {
    manifest_version: &'a str,
    identity: &'a ArtifactIdentity,
    package_id: &'a str,
    min_os_version: &'a str,
    required_agent_capabilities: &'a [String],
    sha256: ArtifactSha256,
    size_bytes: u64,
    publisher: SigningPublisher<'a>,
}

#[derive(Serialize)]
struct SigningPublisher<'a> {
    algorithm: &'a str,
    key_id: &'a str,
}

/// Only used to produce a distinct error for a newer/older format before the
/// strict struct (which would call every v2 field "unknown") runs.
#[derive(Deserialize)]
struct VersionProbe {
    manifest_version: String,
}

fn signing_bytes(claims: &ManifestClaims, key_id: &str) -> Vec<u8> {
    let view = SigningView {
        manifest_version: MANIFEST_VERSION,
        identity: &claims.identity,
        package_id: &claims.package_id,
        min_os_version: &claims.min_os_version,
        required_agent_capabilities: &claims.required_agent_capabilities,
        sha256: claims.integrity.sha256,
        size_bytes: claims.integrity.size_bytes,
        publisher: SigningPublisher {
            algorithm: ALGORITHM_ED25519,
            key_id,
        },
    };
    let mut out = SIGNING_DOMAIN.to_vec();
    serde_json::to_writer(&mut out, &view).expect("signing view serializes");
    out
}

// ------------------------------------------------------------ sign/parse

/// A manifest signed by a publisher key, ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedManifest {
    claims: ManifestClaims,
    key_id: PublisherKeyId,
    signature: [u8; 64],
}

impl SignedManifest {
    pub fn claims(&self) -> &ManifestClaims {
        &self.claims
    }
    pub fn key_id(&self) -> &PublisherKeyId {
        &self.key_id
    }

    /// Canonical file bytes: compact JSON plus one trailing newline.
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let c = &self.claims;
        let wire = WireManifest {
            manifest_version: MANIFEST_VERSION.to_owned(),
            identity: c.identity.clone(),
            package_id: c.package_id.clone(),
            min_os_version: c.min_os_version.clone(),
            required_agent_capabilities: c.required_agent_capabilities.clone(),
            sha256: c.integrity.sha256,
            size_bytes: c.integrity.size_bytes,
            publisher: WirePublisher {
                algorithm: ALGORITHM_ED25519.to_owned(),
                key_id: self.key_id.0.clone(),
                signature: STANDARD.encode(self.signature),
            },
        };
        let mut out = serde_json::to_vec(&wire).expect("manifest serializes");
        out.push(b'\n');
        out
    }
}

pub fn sign_manifest(claims: ManifestClaims, key: &PublisherSigningKey) -> SignedManifest {
    let key_id = PublisherKeyId::of(&key.0.verifying_key());
    let signature = key
        .0
        .sign(&signing_bytes(&claims, key_id.as_str()))
        .to_bytes();
    SignedManifest {
        claims,
        key_id,
        signature,
    }
}

/// Strict, signature-unchecked parse. Private: callers must go through
/// [`verify_offline`] so an unverified manifest is never usable.
fn parse_strict(bytes: &[u8]) -> Result<(ManifestClaims, String, [u8; 64]), ManifestError> {
    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    if probe.manifest_version != MANIFEST_VERSION {
        return Err(ManifestError::UnsupportedVersion(probe.manifest_version));
    }
    let wire: WireManifest =
        serde_json::from_slice(bytes).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    if wire.publisher.algorithm != ALGORITHM_ED25519 {
        return Err(ManifestError::UnsupportedAlgorithm(
            wire.publisher.algorithm,
        ));
    }
    let signature = decode_b64_exact::<64>(&wire.publisher.signature)
        .ok_or_else(|| invalid("publisher.signature must be padded base64 of exactly 64 bytes"))?;
    let claims = ManifestClaims::new(
        wire.identity,
        wire.package_id,
        wire.min_os_version,
        wire.required_agent_capabilities,
        ArtifactIntegrity {
            sha256: wire.sha256,
            size_bytes: wire.size_bytes,
        },
    )?;
    let candidate = SignedManifest {
        claims,
        key_id: PublisherKeyId(wire.publisher.key_id.clone()),
        signature,
    };
    if candidate.to_canonical_bytes() != bytes
        && candidate.to_canonical_bytes().strip_suffix(b"\n") != Some(bytes)
    {
        return Err(ManifestError::NonCanonical);
    }
    Ok((candidate.claims, wire.publisher.key_id, signature))
}

/// The `publisher.key_id` a manifest *claims*, without verifying anything.
/// Only for selecting which managed trusted key to verify against; the claim
/// confers no trust and `verify_offline` still checks it against that key.
pub fn claimed_key_id(manifest_bytes: &[u8]) -> Result<String, ManifestError> {
    parse_strict(manifest_bytes).map(|(_, key_id, _)| key_id)
}

// ----------------------------------------------------------------- verify

/// Result of a successful offline check. Signature and integrity only: it says
/// nothing about current publisher authorisation, freshness or revocation, and
/// has no conversion to any deploy-ready state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineVerified {
    claims: ManifestClaims,
    key_id: PublisherKeyId,
}

impl OfflineVerified {
    pub fn claims(&self) -> &ManifestClaims {
        &self.claims
    }
    pub fn key_id(&self) -> &PublisherKeyId {
        &self.key_id
    }
}

/// Verify `manifest_bytes` against an independently supplied key, the
/// identity the caller expects, and the observed artifact integrity.
pub fn verify_offline(
    manifest_bytes: &[u8],
    trusted: &TrustedPublisherKey,
    expected_identity: &ArtifactIdentity,
    observed: &ArtifactIntegrity,
) -> Result<OfflineVerified, ManifestError> {
    let (claims, key_id, signature) = parse_strict(manifest_bytes)?;
    if key_id != trusted.key_id.0 {
        return Err(ManifestError::KeyIdMismatch);
    }
    trusted
        .key
        .verify_strict(
            &signing_bytes(&claims, &key_id),
            &Signature::from_bytes(&signature),
        )
        .map_err(|_| ManifestError::SignatureInvalid)?;
    if &claims.identity != expected_identity {
        return Err(ManifestError::IdentityMismatch);
    }
    if claims.integrity.size_bytes != observed.size_bytes {
        return Err(ManifestError::SizeMismatch);
    }
    if claims.integrity.sha256 != observed.sha256 {
        return Err(ManifestError::DigestMismatch);
    }
    Ok(OfflineVerified {
        claims,
        key_id: trusted.key_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{ArtifactRole, DistroAbi, PackageFormat, TargetArch, TargetOs};
    use crate::signing;

    const BYTES: &[u8] = b"package bytes";

    fn identity_for(role: ArtifactRole, version: &str) -> ArtifactIdentity {
        ArtifactIdentity::new(
            role,
            version.parse().unwrap(),
            TargetOs::Macos,
            TargetArch::Aarch64,
            DistroAbi::Darwin,
            PackageFormat::TarGz,
        )
        .unwrap()
    }

    fn key(n: u8) -> PublisherSigningKey {
        PublisherSigningKey::from_seed([n; 32])
    }

    fn claims_with(id: ArtifactIdentity) -> ManifestClaims {
        ManifestClaims::new(
            id,
            "com.kanade.cli".into(),
            "14.0".into(),
            vec!["a_cap".into(), "b_cap".into()],
            ArtifactIntegrity::of(BYTES),
        )
        .unwrap()
    }

    fn id() -> ArtifactIdentity {
        identity_for(ArtifactRole::Cli, "1.2.3")
    }

    fn signed() -> Vec<u8> {
        sign_manifest(claims_with(id()), &key(7)).to_canonical_bytes()
    }

    fn verify(bytes: &[u8]) -> Result<OfflineVerified, ManifestError> {
        verify_offline(
            bytes,
            &key(7).trusted_key(),
            &id(),
            &ArtifactIntegrity::of(BYTES),
        )
    }

    /// Textual edit of the canonical manifest, so the result stays canonical
    /// and only the signature can reject it.
    fn swap(from: &str, to: &str) -> Vec<u8> {
        let s = String::from_utf8(signed()).unwrap();
        assert_eq!(s.matches(from).count(), 1, "{from}");
        s.replace(from, to).into_bytes()
    }

    /// Structural edit through `Value` (key order is not preserved, so only
    /// for inputs that must be rejected for any reason).
    fn edit(f: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::from_slice(&signed()).unwrap();
        f(&mut v);
        let mut out = serde_json::to_vec(&v).unwrap();
        out.push(b'\n');
        out
    }

    #[test]
    fn round_trip_with_real_keypair() {
        let ok = verify(&signed()).unwrap();
        assert_eq!(ok.claims().package_id(), "com.kanade.cli");
        assert_eq!(ok.key_id(), key(7).trusted_key().key_id());
        // The trailing newline is optional.
        let bytes = signed();
        verify(bytes.strip_suffix(b"\n").unwrap()).unwrap();
    }

    #[test]
    fn random_temporary_keypairs_round_trip_and_reject_wrong_key() {
        let random_key = || {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).unwrap();
            PublisherSigningKey::from_seed(seed)
        };
        let key = random_key();
        let wrong = random_key();
        let manifest = sign_manifest(claims_with(id()), &key).to_canonical_bytes();
        let trust =
            TrustedPublisherKey::from_trust_json(&key.trusted_key().to_trust_json()).unwrap();
        let observed = ArtifactIntegrity::of(BYTES);
        verify_offline(&manifest, &trust, &id(), &observed).unwrap();
        assert_eq!(
            verify_offline(&manifest, &wrong.trusted_key(), &id(), &observed).unwrap_err(),
            ManifestError::KeyIdMismatch
        );
    }

    #[test]
    fn weak_trust_keys_are_rejected_even_with_matching_key_id() {
        for raw in [[0u8; 32], {
            let mut identity = [0u8; 32];
            identity[0] = 1;
            identity
        }] {
            let key = VerifyingKey::from_bytes(&raw).unwrap();
            assert!(key.is_weak());
            let record = serde_json::json!({
                "purpose": TRUST_PURPOSE,
                "algorithm": ALGORITHM_ED25519,
                "key_id": PublisherKeyId::of(&key).as_str(),
                "public_key": STANDARD.encode(raw),
            });
            assert_eq!(
                TrustedPublisherKey::from_trust_json(&serde_json::to_vec(&record).unwrap())
                    .unwrap_err(),
                ManifestError::Trust("weak publisher public key".into())
            );
            assert!(TrustedPublisherKey::from_verifying_key(key).is_err());
        }
    }

    #[test]
    fn signing_is_deterministic_and_bytes_are_pinned() {
        assert_eq!(signed(), signed());
        let k = key(7);
        let c = claims_with(id());
        let bytes = signing_bytes(&c, k.trusted_key().key_id().as_str());
        let expect = concat!(
            "kanade/package-manifest/ed25519/v1\0",
            r#"{"manifest_version":"1.0","identity":{"role":"cli","version":"1.2.3","os":"macos","arch":"aarch64","distro_abi":"darwin","format":"tar_gz"},"package_id":"com.kanade.cli","min_os_version":"14.0","required_agent_capabilities":["a_cap","b_cap"],"sha256":"2e547448dcd0f2fcd9dbc386d33f1553369883451898177559bcf3e3b1083d16","size_bytes":13,"publisher":{"algorithm":"ed25519","key_id":"ed25519:c283f73433913d171beef378f847ac83bb4bfe58359cd7a7dd6d295b13562b87"}}"#
        );
        assert_eq!(bytes, expect.as_bytes());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let other = key(8).trusted_key();
        let r = verify_offline(&signed(), &other, &id(), &ArtifactIntegrity::of(BYTES));
        assert_eq!(r.unwrap_err(), ManifestError::KeyIdMismatch);
    }

    #[test]
    fn embedded_key_id_cannot_select_trust() {
        // Attacker signs with their own key and claims their own key_id:
        // the independently supplied key still decides.
        let trusted_id = key(7).trusted_key().key_id().as_str().to_owned();
        let forged = sign_manifest(claims_with(id()), &key(9)).to_canonical_bytes();
        assert_eq!(verify(&forged).unwrap_err(), ManifestError::KeyIdMismatch);
        // Rewriting key_id to the trusted one breaks the signature.
        let text = String::from_utf8(forged).unwrap();
        let attacker_id = key(9).trusted_key().key_id().as_str().to_owned();
        let forged = text.replace(&attacker_id, &trusted_id).into_bytes();
        assert_eq!(
            verify(&forged).unwrap_err(),
            ManifestError::SignatureInvalid
        );
    }

    #[test]
    fn every_signed_field_is_covered_by_the_signature() {
        let digest = String::from(ArtifactIntegrity::of(BYTES).sha256);
        let zeros = "0".repeat(64);
        let cases: Vec<(&str, &str, &str)> = vec![
            ("role", "\"role\":\"cli\"", "\"role\":\"agent\""),
            ("version", "\"version\":\"1.2.3\"", "\"version\":\"1.2.4\""),
            ("os", "\"os\":\"macos\"", "\"os\":\"linux\""),
            ("arch", "\"arch\":\"aarch64\"", "\"arch\":\"x86_64\""),
            (
                "abi",
                "\"distro_abi\":\"darwin\"",
                "\"distro_abi\":\"musl\"",
            ),
            ("format", "\"format\":\"tar_gz\"", "\"format\":\"zip\""),
            ("package_id", "com.kanade.cli", "com.evil"),
            ("min_os", "\"14.0\"", "\"13.0\""),
            ("caps", "[\"a_cap\",\"b_cap\"]", "[\"a_cap\"]"),
            ("sha256", digest.as_str(), zeros.as_str()),
            ("size", "\"size_bytes\":13", "\"size_bytes\":14"),
        ];
        for (name, from, to) in cases {
            let err = verify(&swap(from, to)).unwrap_err();
            // Combinations the identity validator forbids fail earlier; every
            // other edit must be caught by the signature itself.
            assert!(
                matches!(
                    err,
                    ManifestError::SignatureInvalid | ManifestError::Malformed(_)
                ),
                "{name}: {err:?}"
            );
        }
        for (name, from, to) in [
            ("role", "\"role\":\"cli\"", "\"role\":\"agent\""),
            ("version", "\"version\":\"1.2.3\"", "\"version\":\"1.2.4\""),
            ("package_id", "com.kanade.cli", "com.evil"),
            ("min_os", "\"14.0\"", "\"13.0\""),
            ("caps", "[\"a_cap\",\"b_cap\"]", "[\"a_cap\"]"),
            ("sha256", digest.as_str(), zeros.as_str()),
            ("size", "\"size_bytes\":13", "\"size_bytes\":14"),
        ] {
            assert_eq!(
                verify(&swap(from, to)).unwrap_err(),
                ManifestError::SignatureInvalid,
                "{name}"
            );
        }
    }

    #[test]
    fn cross_role_version_platform_substitution_is_rejected() {
        // A genuinely signed manifest for another role/version/platform.
        let other_role = sign_manifest(
            claims_with(identity_for(ArtifactRole::Agent, "1.2.3")),
            &key(7),
        );
        let other_ver = sign_manifest(
            claims_with(identity_for(ArtifactRole::Cli, "1.2.4")),
            &key(7),
        );
        let other_plat = sign_manifest(
            claims_with(
                ArtifactIdentity::new(
                    ArtifactRole::Cli,
                    "1.2.3".parse().unwrap(),
                    TargetOs::Linux,
                    TargetArch::Aarch64,
                    DistroAbi::Musl,
                    PackageFormat::TarGz,
                )
                .unwrap(),
            ),
            &key(7),
        );
        for m in [other_role, other_ver, other_plat] {
            assert_eq!(
                verify(&m.to_canonical_bytes()).unwrap_err(),
                ManifestError::IdentityMismatch
            );
        }
    }

    #[test]
    fn artifact_size_and_digest_tampering_is_rejected() {
        let m = signed();
        let key = key(7).trusted_key();
        let mut flipped = BYTES.to_vec();
        flipped[0] ^= 1;
        assert_eq!(
            verify_offline(&m, &key, &id(), &ArtifactIntegrity::of(&flipped)).unwrap_err(),
            ManifestError::DigestMismatch
        );
        assert_eq!(
            verify_offline(&m, &key, &id(), &ArtifactIntegrity::of(&BYTES[..5])).unwrap_err(),
            ManifestError::SizeMismatch
        );
    }

    #[test]
    fn malformed_truncated_duplicate_and_trailing_input_is_rejected() {
        let good = String::from_utf8(signed()).unwrap();
        let trimmed = good.trim_end();
        for bad in [
            String::new(),
            "{}".into(),
            "null".into(),
            trimmed[..trimmed.len() / 2].to_owned(),
            trimmed[..trimmed.len() - 1].to_owned(),
            format!("{trimmed}x"),
            format!("{trimmed}{trimmed}"),
            format!("{trimmed}\n\n"),
            format!(" {trimmed}"),
            // duplicate top-level field
            trimmed.replacen("{", "{\"package_id\":\"x\",", 1),
            // unknown field
            trimmed.replacen("{", "{\"extra\":1,", 1),
            // unknown nested field
            trimmed.replacen("\"publisher\":{", "\"publisher\":{\"extra\":1,", 1),
            // duplicate nested identity field
            trimmed.replacen("\"identity\":{", "\"identity\":{\"role\":\"cli\",", 1),
        ] {
            assert!(verify(bad.as_bytes()).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn non_canonical_spelling_is_rejected_even_when_semantically_equal() {
        let v: serde_json::Value = serde_json::from_slice(&signed()).unwrap();
        let pretty = serde_json::to_vec_pretty(&v).unwrap();
        assert_eq!(verify(&pretty).unwrap_err(), ManifestError::NonCanonical);
        let spaced = String::from_utf8(signed()).unwrap().replacen(":", ": ", 1);
        assert_eq!(
            verify(spaced.as_bytes()).unwrap_err(),
            ManifestError::NonCanonical
        );
        let crlf = String::from_utf8(signed()).unwrap().replace('\n', "\r\n");
        assert_eq!(
            verify(crlf.as_bytes()).unwrap_err(),
            ManifestError::NonCanonical
        );
    }

    #[test]
    fn missing_fields_and_null_min_os_are_rejected() {
        for field in [
            "manifest_version",
            "identity",
            "package_id",
            "min_os_version",
            "required_agent_capabilities",
            "sha256",
            "size_bytes",
            "publisher",
        ] {
            let b = edit(|v| {
                v.as_object_mut().unwrap().remove(field);
            });
            assert!(verify(&b).is_err(), "missing {field} accepted");
        }
        assert!(matches!(
            verify(&swap(
                "\"min_os_version\":\"14.0\"",
                "\"min_os_version\":null"
            )),
            Err(ManifestError::Malformed(_))
        ));
        let b = edit(|v| {
            v["publisher"].as_object_mut().unwrap().remove("algorithm");
        });
        assert!(verify(&b).is_err());
    }

    #[test]
    fn unsupported_version_and_algorithm_are_distinct_errors() {
        let b = edit(|v| v["manifest_version"] = "2.0".into());
        assert_eq!(
            verify(&b).unwrap_err(),
            ManifestError::UnsupportedVersion("2.0".into())
        );
        // A future version with new fields is still "unsupported version".
        let b = edit(|v| {
            v["manifest_version"] = "2.0".into();
            v["new_field"] = 1.into();
        });
        assert!(matches!(
            verify(&b).unwrap_err(),
            ManifestError::UnsupportedVersion(_)
        ));
        for alg in ["ed448", "ED25519", ""] {
            let b = edit(|v| v["publisher"]["algorithm"] = alg.into());
            assert!(
                matches!(
                    verify(&b).unwrap_err(),
                    ManifestError::UnsupportedAlgorithm(_)
                ),
                "{alg}"
            );
        }
    }

    #[test]
    fn invalid_signature_encoding_is_rejected() {
        let good: serde_json::Value = serde_json::from_slice(&signed()).unwrap();
        let sig = good["publisher"]["signature"].as_str().unwrap().to_owned();
        let url_safe = sig.replace('+', "-").replace('/', "_");
        for bad in [
            String::new(),
            "not base64!!".into(),
            sig.trim_end_matches('=').to_owned(),
            format!("{sig}A"),
            STANDARD.encode([0u8; 63]),
            STANDARD.encode([0u8; 65]),
            format!(" {sig}"),
            url_safe,
        ] {
            if bad == sig {
                continue;
            }
            let b = edit(|v| v["publisher"]["signature"] = bad.clone().into());
            assert!(verify(&b).is_err(), "accepted signature {bad:?}");
        }
        // All-zero 64 bytes: well-formed encoding, wrong signature.
        assert_eq!(
            verify(&swap(&sig, &STANDARD.encode([0u8; 64]))).unwrap_err(),
            ManifestError::SignatureInvalid
        );
    }

    #[test]
    fn compatibility_fields_are_validated() {
        let mk = |pid: &str, os: &str, caps: &[&str]| {
            ManifestClaims::new(
                id(),
                pid.into(),
                os.into(),
                caps.iter().map(|s| s.to_string()).collect(),
                ArtifactIntegrity::of(BYTES),
            )
        };
        assert!(mk("com.kanade.client", "24.04", &["a", "b"]).is_ok());
        assert!(mk("com.kanade.client", "", &[]).is_err());
        for pid in ["", "Com.x", ".x", "a b", "a/b", &"a".repeat(129)] {
            assert!(mk(pid, "14.0", &[]).is_err(), "{pid:?}");
        }
        for os in ["", "14.", ".14", "14.x", "1.2.3.4.5", "1234567890", "v14"] {
            assert!(mk("x", os, &[]).is_err(), "{os:?}");
        }
        assert!(mk("x", "14.0", &["b", "a"]).is_err());
        assert!(mk("x", "14.0", &["a", "a"]).is_err());
        assert!(mk("x", "14.0", &["A"]).is_err());
        assert!(mk("x", "14.0", &[""]).is_err());
        let empty = ManifestClaims::new(
            id(),
            "x".into(),
            "14.0".into(),
            vec![],
            ArtifactIntegrity::of(b""),
        );
        assert!(empty.is_err());
    }

    #[test]
    fn command_signing_authority_is_not_package_publisher_authority() {
        // Same secret used for a command signature: the command-signed material
        // (timestamp || body) is not a valid manifest signature, and the command
        // signing key is not accepted just because its public key is Ed25519.
        let seed = [7u8; 32];
        let cmd_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let c = claims_with(id());
        let key_id = key(7).trusted_key().key_id().clone();
        let body = signing_bytes(&c, key_id.as_str());
        let headers = signing::sign(&cmd_key, "backend-1", &body, 0);
        let sig = STANDARD.decode(headers.sig_b64.unwrap()).unwrap();
        let genuine = STANDARD.encode(sign_manifest(claims_with(id()), &key(7)).signature);
        let forged = swap(&genuine, &STANDARD.encode(&sig));
        assert_eq!(
            verify(&forged).unwrap_err(),
            ManifestError::SignatureInvalid
        );

        // And a manifest signature never verifies as a command signature.
        let m = sign_manifest(claims_with(id()), &key(7));
        let mut ring = signing::KeyRing::new();
        ring.insert(
            "k",
            cmd_key.verifying_key(),
            signing::KeyPolicy::backend("t"),
        );
        let hdr = signing::SigHeaders {
            sig_b64: Some(STANDARD.encode(m.signature)),
            kid: Some("k".into()),
            alg: Some(signing::ALG_ED25519.into()),
            at_ms: Some("0".into()),
        };
        assert!(signing::verify_signature(&ring, &body, &hdr).is_err());
        // Domain prefix is what separates the byte strings.
        assert!(body.starts_with(SIGNING_DOMAIN));
    }

    #[test]
    fn trust_input_is_strict_and_purpose_bound() {
        let t = key(7).trusted_key();
        let json = t.to_trust_json();
        assert_eq!(TrustedPublisherKey::from_trust_json(&json).unwrap(), t);
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let with = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v = v.clone();
            f(&mut v);
            serde_json::to_vec(&v).unwrap()
        };
        let bad = [
            with(&|v| v["purpose"] = "kanade-command-key".into()),
            with(&|v| v["algorithm"] = "rsa".into()),
            with(&|v| v["key_id"] = key(8).trusted_key().key_id().as_str().into()),
            with(&|v| v["public_key"] = STANDARD.encode([1u8; 31]).into()),
            with(&|v| v["extra"] = 1.into()),
            with(&|v| {
                v.as_object_mut().unwrap().remove("purpose");
            }),
            b"{".to_vec(),
        ];
        for b in bad {
            assert!(TrustedPublisherKey::from_trust_json(&b).is_err());
        }
        // A command-signing keyring entry is not a trust input.
        let entry = signing::keyring_entry("k", &key(7).0.verifying_key(), "x");
        assert!(TrustedPublisherKey::from_trust_json(entry.to_string().as_bytes()).is_err());
    }

    #[test]
    fn private_key_input_is_strict_and_never_printed() {
        let seed_b64 = STANDARD.encode([7u8; 32]);
        let k = PublisherSigningKey::from_seed_base64(&format!("{seed_b64}\n")).unwrap();
        assert_eq!(k.trusted_key(), key(7).trusted_key());
        assert!(!format!("{k:?}").contains(&seed_b64));
        let err = PublisherSigningKey::from_seed_base64("hunter2-secret").unwrap_err();
        assert!(!err.to_string().contains("hunter2"));
        assert!(PublisherSigningKey::from_seed_base64(&STANDARD.encode([1u8; 31])).is_err());
        assert!(PublisherSigningKey::from_seed_base64(seed_b64.trim_end_matches('=')).is_err());
    }
}
