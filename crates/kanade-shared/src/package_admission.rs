//! Agent-side admission of a package against its signed manifest
//! (SPEC §2.13.4.3, stage 2b).
//!
//! [`admit_package`] answers one question — "is this exact artifact something a
//! *currently authorised* publisher signed, and is it compatible with this
//! host?" — and nothing more. It combines, all of which must hold:
//!
//! 1. the artifact bytes equal the externally expected size and SHA-256 (the
//!    expectation comes from the request being served, never from the
//!    manifest or the package);
//! 2. the manifest's claimed `publisher.key_id` is authorised **now** by the
//!    managed trust state ([`PublisherTrustStore::authorize`]: pinned root,
//!    freshness, active, unrevoked, inside its window, clock certain);
//! 3. [`verify_offline`] succeeds with that managed key against the externally
//!    expected full [`ArtifactIdentity`] and the observed integrity;
//! 4. the signed `package_id` is the expected one;
//! 5. the host satisfies the signed compatibility claims: platform, numeric
//!    minimum OS version, and required agent capabilities.
//!
//! The result, [`AdmittedPackage`], is evidence bound to that identity, artifact
//! and manifest digests, and trust sequence. It is **not** an install
//! authorisation: nothing here installs, stages or executes anything, and typed handlers still need their
//! own privilege checks. It cannot be built from [`OfflineVerified`]
//! (signature + integrity only) or from any other type.
//!
//! The manifest carries no signing time, so it has no freshness of its own:
//! rollback to an older signed package is prevented by the caller's expected
//! identity (including version), and a compromised publisher key is cut off by
//! revocation in the trust state, not by anything in the manifest.

use sha2::{Digest, Sha256};

use crate::artifact::{ArtifactIdentity, ArtifactIntegrity, TargetArch, TargetOs};
use crate::package_manifest::{
    ManifestClaims, ManifestError, PublisherKeyId, claimed_key_id, validate_min_os_version,
    verify_offline,
};
use crate::publisher_trust::{PublisherTrustStore, TrustError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    /// Trust state missing, stale, revoked/unknown publisher, clock, storage.
    #[error("publisher not authorized: {0}")]
    Trust(#[from] TrustError),
    #[error("manifest verification failed: {0}")]
    Manifest(#[from] ManifestError),
    #[error("manifest package_id differs from the expected package_id")]
    PackageIdMismatch,
    #[error("package targets a different platform than this host")]
    PlatformMismatch,
    #[error("host OS version is below the package's min_os_version")]
    OsVersionTooLow,
    #[error("host lacks required agent capability {0:?}")]
    MissingCapability(String),
    #[error("invalid compatibility policy: {0}")]
    Policy(String),
}

/// What this host offers. Required for every admission: there is no way to
/// skip compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityPolicy {
    os: TargetOs,
    arch: TargetArch,
    os_version: String,
    capabilities: Vec<String>,
}

impl CompatibilityPolicy {
    /// `os_version` uses the manifest's `min_os_version` syntax.
    pub fn new(
        os: TargetOs,
        arch: TargetArch,
        os_version: String,
        capabilities: Vec<String>,
    ) -> Result<Self, AdmissionError> {
        validate_min_os_version(&os_version)
            .map_err(|_| AdmissionError::Policy("os_version syntax".into()))?;
        Ok(Self {
            os,
            arch,
            os_version,
            capabilities,
        })
    }

    fn check(&self, claims: &ManifestClaims) -> Result<(), AdmissionError> {
        let id = claims.identity();
        if id.os() != self.os || id.arch() != self.arch {
            return Err(AdmissionError::PlatformMismatch);
        }
        if version_parts(&self.os_version) < version_parts(claims.min_os_version()) {
            return Err(AdmissionError::OsVersionTooLow);
        }
        if let Some(missing) = claims
            .required_agent_capabilities()
            .iter()
            .find(|c| !self.capabilities.contains(c))
        {
            return Err(AdmissionError::MissingCapability(missing.clone()));
        }
        Ok(())
    }
}

/// Numeric dotted version padded to 4 groups (so `14` == `14.0.0.0`).
fn version_parts(v: &str) -> [u64; 4] {
    let mut out = [0u64; 4];
    for (slot, part) in out.iter_mut().zip(v.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    out
}

/// Everything the caller must supply from outside the package.
pub struct AdmissionRequest<'a> {
    pub manifest_bytes: &'a [u8],
    pub artifact_bytes: &'a [u8],
    pub expected_identity: &'a ArtifactIdentity,
    pub expected_integrity: &'a ArtifactIntegrity,
    pub expected_package_id: &'a str,
    pub policy: &'a CompatibilityPolicy,
}

/// Evidence binding one exact artifact and manifest to one trust sequence. No public constructor; not an install authorisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedPackage {
    claims: ManifestClaims,
    publisher: PublisherKeyId,
    trust_set_id: String,
    trust_sequence: u64,
    manifest_sha256: [u8; 32],
}

impl AdmittedPackage {
    /// SHA-256 of the exact manifest bytes checked during admission.
    pub fn manifest_sha256(&self) -> &[u8; 32] {
        &self.manifest_sha256
    }
    pub fn claims(&self) -> &ManifestClaims {
        &self.claims
    }
    pub fn identity(&self) -> &ArtifactIdentity {
        self.claims.identity()
    }
    pub fn integrity(&self) -> &ArtifactIntegrity {
        self.claims.integrity()
    }
    pub fn publisher(&self) -> &PublisherKeyId {
        &self.publisher
    }
    pub fn trust_set_id(&self) -> &str {
        &self.trust_set_id
    }
    pub fn trust_sequence(&self) -> u64 {
        self.trust_sequence
    }
}

/// Fails closed: any error means "do not admit this package". Unrelated
/// agent operation never calls this and is unaffected.
pub fn admit_package(
    store: &PublisherTrustStore,
    req: &AdmissionRequest<'_>,
) -> Result<AdmittedPackage, AdmissionError> {
    let observed = ArtifactIntegrity::of(req.artifact_bytes);
    if observed.size_bytes != req.expected_integrity.size_bytes {
        return Err(ManifestError::SizeMismatch.into());
    }
    if observed.sha256 != req.expected_integrity.sha256 {
        return Err(ManifestError::DigestMismatch.into());
    }
    // The claimed id only *selects* a managed key; it grants nothing.
    let claimed = claimed_key_id(req.manifest_bytes)?;
    let authorized = store.authorize(&claimed)?;
    let ok = verify_offline(
        req.manifest_bytes,
        authorized.key(),
        req.expected_identity,
        &observed,
    )?;
    if ok.claims().package_id() != req.expected_package_id {
        return Err(AdmissionError::PackageIdMismatch);
    }
    req.policy.check(ok.claims())?;
    Ok(AdmittedPackage {
        manifest_sha256: Sha256::digest(req.manifest_bytes).into(),
        claims: ok.claims().clone(),
        publisher: ok.key_id().clone(),
        trust_set_id: authorized.trust_set_id().to_owned(),
        trust_sequence: authorized.trust_sequence(),
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(unix)]
    use std::time::Duration;

    use super::*;
    use crate::artifact::{ArtifactRole, DistroAbi, PackageFormat};
    use crate::package_manifest::{PublisherSigningKey, sign_manifest};
    #[cfg(unix)]
    use crate::publisher_trust::{
        PublisherEntry, TrustRoot, TrustStateClaims, WallClock, sign_trust_state,
    };

    #[cfg(unix)]
    const T0: u64 = 1_800_000_000;
    #[cfg(unix)]
    const DAY: u64 = 86_400;
    const BYTES: &[u8] = b"package bytes";

    #[cfg(unix)]
    struct Clock(AtomicU64);
    #[cfg(unix)]
    impl WallClock for Clock {
        fn unix_now(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        fn monotonic(&self) -> Duration {
            Duration::from_secs(self.0.load(Ordering::SeqCst) - T0)
        }
    }

    fn key() -> PublisherSigningKey {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        PublisherSigningKey::from_seed(seed)
    }

    fn identity(version: &str) -> ArtifactIdentity {
        ArtifactIdentity::new(
            ArtifactRole::Cli,
            version.parse().unwrap(),
            TargetOs::Macos,
            TargetArch::Aarch64,
            DistroAbi::Darwin,
            PackageFormat::TarGz,
        )
        .unwrap()
    }

    fn policy(os_version: &str, caps: &[&str]) -> CompatibilityPolicy {
        CompatibilityPolicy::new(
            TargetOs::Macos,
            TargetArch::Aarch64,
            os_version.into(),
            caps.iter().map(|s| (*s).to_owned()).collect(),
        )
        .unwrap()
    }

    fn manifest(k: &PublisherSigningKey, id: ArtifactIdentity, bytes: &[u8]) -> Vec<u8> {
        let claims = ManifestClaims::new(
            id,
            "com.kanade.cli".into(),
            "14.0".into(),
            vec!["a_cap".into()],
            ArtifactIntegrity::of(bytes),
        )
        .unwrap();
        sign_manifest(claims, k).to_canonical_bytes()
    }

    #[cfg(unix)]
    struct Env {
        _dir: tempfile::TempDir,
        store: PublisherTrustStore,
        root: PublisherSigningKey,
        publisher: PublisherSigningKey,
    }

    #[cfg(unix)]
    impl Env {
        /// A store with a pinned root and a state activating `publisher`.
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = key();
            let publisher = key();
            let store = PublisherTrustStore::open(
                dir.path().canonicalize().unwrap().join("trust"),
                Arc::new(Clock(AtomicU64::new(T0))),
            );
            store
                .provision_root(&TrustRoot::from_signing_key(&root).to_root_json())
                .unwrap();
            let me = Self {
                _dir: dir,
                store,
                root,
                publisher,
            };
            me.apply(1, &[&me.publisher], &[]);
            me
        }
        fn apply(
            &self,
            seq: u64,
            active: &[&PublisherSigningKey],
            revoked: &[&PublisherSigningKey],
        ) {
            let mut pubs: Vec<_> = active
                .iter()
                .map(|k| PublisherEntry::new(k.trusted_key(), T0 - DAY, T0 + 20 * DAY).unwrap())
                .collect();
            pubs.sort_by(|a, b| a.key().key_id().as_str().cmp(b.key().key_id().as_str()));
            let mut rev: Vec<String> = revoked
                .iter()
                .map(|k| k.trusted_key().key_id().as_str().to_owned())
                .collect();
            rev.sort();
            let c =
                TrustStateClaims::new("fleet".into(), seq, T0, T0 + 30 * DAY, pubs, rev).unwrap();
            self.store.apply(&sign_trust_state(&c, &self.root)).unwrap();
        }
        fn admit(
            &self,
            manifest: &[u8],
            artifact: &[u8],
            id: &ArtifactIdentity,
            pol: &CompatibilityPolicy,
        ) -> Result<AdmittedPackage, AdmissionError> {
            admit_package(
                &self.store,
                &AdmissionRequest {
                    manifest_bytes: manifest,
                    artifact_bytes: artifact,
                    expected_identity: id,
                    expected_integrity: &ArtifactIntegrity::of(BYTES),
                    expected_package_id: "com.kanade.cli",
                    policy: pol,
                },
            )
        }
    }

    #[cfg(unix)]
    #[test]
    fn authorized_publisher_with_matching_everything_is_admitted() {
        let env = Env::new();
        let id = identity("1.2.3");
        let m = manifest(&env.publisher, id.clone(), BYTES);
        let a = env
            .admit(&m, BYTES, &id, &policy("14.2.1", &["a_cap", "z"]))
            .unwrap();
        assert_eq!(a.manifest_sha256(), &<[u8; 32]>::from(Sha256::digest(&m)));
        let without_newline = m.strip_suffix(b"\n").unwrap();
        let other = env
            .admit(
                without_newline,
                BYTES,
                &id,
                &policy("14.2.1", &["a_cap", "z"]),
            )
            .unwrap();
        assert_eq!(a.claims(), other.claims());
        assert_ne!(a.manifest_sha256(), other.manifest_sha256());
        assert_eq!(a.identity(), &id);
        assert_eq!(a.trust_sequence(), 1);
        assert_eq!(a.publisher(), env.publisher.trusted_key().key_id());
    }

    #[cfg(unix)]
    #[test]
    fn self_signed_unknown_publisher_cannot_establish_its_own_trust() {
        let env = Env::new();
        let id = identity("1.2.3");
        let attacker = key();
        let m = manifest(&attacker, id.clone(), BYTES);
        assert_eq!(
            env.admit(&m, BYTES, &id, &policy("14.0", &["a_cap"]))
                .unwrap_err(),
            AdmissionError::Trust(TrustError::UnknownPublisher)
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_style_or_root_keys_signing_a_manifest_are_not_publishers() {
        // The root key is a different authority from any publisher: a manifest
        // signed by it is refused as an unknown publisher.
        let env = Env::new();
        let id = identity("1.2.3");
        let m = manifest(&env.root, id.clone(), BYTES);
        assert_eq!(
            env.admit(&m, BYTES, &id, &policy("14.0", &["a_cap"]))
                .unwrap_err(),
            AdmissionError::Trust(TrustError::UnknownPublisher)
        );
    }

    #[cfg(unix)]
    #[test]
    fn revocation_and_rotation_take_effect_for_new_admissions() {
        let env = Env::new();
        let id = identity("1.2.3");
        let new = key();
        let old_m = manifest(&env.publisher, id.clone(), BYTES);
        let new_m = manifest(&new, id.clone(), BYTES);
        let pol = policy("14.0", &["a_cap"]);
        env.apply(2, &[&env.publisher, &new], &[]);
        env.admit(&old_m, BYTES, &id, &pol).unwrap();
        env.admit(&new_m, BYTES, &id, &pol).unwrap();
        env.apply(3, &[&new], &[&env.publisher]);
        assert_eq!(
            env.admit(&old_m, BYTES, &id, &pol).unwrap_err(),
            AdmissionError::Trust(TrustError::PublisherRevoked)
        );
        assert_eq!(
            env.admit(&new_m, BYTES, &id, &pol)
                .unwrap()
                .trust_sequence(),
            3
        );
    }

    #[cfg(unix)]
    #[test]
    fn typed_binding_checks_real_admission_and_every_external_reference() {
        use crate::artifact::ArtifactSha256;
        use crate::typed_operation::{DeployCliSpec, TypedOperation};
        let env = Env::new();
        let id = identity("1.2.3");
        let bytes = manifest(&env.publisher, id.clone(), BYTES);
        let admitted = env
            .admit(&bytes, BYTES, &id, &policy("14.0", &["a_cap"]))
            .unwrap();
        let spec = DeployCliSpec {
            package_object_key: id.app_package_key().unwrap(),
            artifact_identity: id,
            artifact_sha256: admitted.integrity().sha256,
            artifact_size_bytes: admitted.integrity().size_bytes,
            package_id: "com.kanade.cli".into(),
            manifest_sha256: ArtifactSha256::of(&bytes),
            adopt_existing_cli: false,
        };
        TypedOperation::DeployCli(spec.clone())
            .check_admission(&admitted)
            .unwrap();
        for field in ["size", "sha", "package_id", "manifest", "identity", "key"] {
            let mut bad = spec.clone();
            match field {
                "size" => bad.artifact_size_bytes += 1,
                "sha" => bad.artifact_sha256 = ArtifactSha256::of(b"different"),
                "package_id" => bad.package_id = "com.other.cli".into(),
                "manifest" => bad.manifest_sha256 = ArtifactSha256::of(b"different"),
                "identity" => {
                    bad.artifact_identity = identity("1.2.4");
                    bad.package_object_key = bad.artifact_identity.app_package_key().unwrap();
                }
                "key" => bad.package_object_key = "wrong/key".into(),
                _ => unreachable!(),
            }
            assert!(
                TypedOperation::DeployCli(bad)
                    .check_admission(&admitted)
                    .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn no_trust_configured_refuses_admission() {
        let dir = tempfile::tempdir().unwrap();
        let store = PublisherTrustStore::open_system(dir.path().join("none"));
        let id = identity("1.2.3");
        let k = key();
        let m = manifest(&k, id.clone(), BYTES);
        let err = admit_package(
            &store,
            &AdmissionRequest {
                manifest_bytes: &m,
                artifact_bytes: BYTES,
                expected_identity: &id,
                expected_integrity: &ArtifactIntegrity::of(BYTES),
                expected_package_id: "com.kanade.cli",
                policy: &policy("14.0", &["a_cap"]),
            },
        )
        .unwrap_err();
        assert_eq!(err, AdmissionError::Trust(TrustError::NoRoot));
    }

    #[cfg(unix)]
    #[test]
    fn tampered_identity_content_package_and_expectations_are_rejected() {
        let env = Env::new();
        let id = identity("1.2.3");
        let m = manifest(&env.publisher, id.clone(), BYTES);
        let pol = policy("14.0", &["a_cap"]);
        // Different expected identity (version downgrade/replay).
        assert_eq!(
            env.admit(&m, BYTES, &identity("1.2.2"), &pol).unwrap_err(),
            AdmissionError::Manifest(ManifestError::IdentityMismatch)
        );
        // Artifact bytes differ from the externally expected digest.
        assert_eq!(
            env.admit(&m, b"package bytez", &id, &pol).unwrap_err(),
            AdmissionError::Manifest(ManifestError::DigestMismatch)
        );
        assert_eq!(
            env.admit(&m, b"short", &id, &pol).unwrap_err(),
            AdmissionError::Manifest(ManifestError::SizeMismatch)
        );
        // Manifest signed for other bytes, but the artifact matches the
        // external expectation: the manifest's digest disagrees.
        let other = manifest(&env.publisher, id.clone(), b"other bytes!");
        assert_eq!(
            env.admit(&other, BYTES, &id, &pol).unwrap_err(),
            AdmissionError::Manifest(ManifestError::SizeMismatch)
        );
        // Manifest text tamper.
        let mut t = String::from_utf8(m.clone()).unwrap();
        t = t.replace("\"min_os_version\":\"14.0\"", "\"min_os_version\":\"1.0\"");
        assert_eq!(
            env.admit(t.as_bytes(), BYTES, &id, &pol).unwrap_err(),
            AdmissionError::Manifest(ManifestError::SignatureInvalid)
        );
        // Wrong expected package id.
        let r = admit_package(
            &env.store,
            &AdmissionRequest {
                manifest_bytes: &m,
                artifact_bytes: BYTES,
                expected_identity: &id,
                expected_integrity: &ArtifactIntegrity::of(BYTES),
                expected_package_id: "com.kanade.other",
                policy: &pol,
            },
        );
        assert_eq!(r.unwrap_err(), AdmissionError::PackageIdMismatch);
    }

    #[cfg(unix)]
    #[test]
    fn compatibility_policy_is_enforced() {
        let env = Env::new();
        let id = identity("1.2.3");
        let m = manifest(&env.publisher, id.clone(), BYTES);
        assert_eq!(
            env.admit(&m, BYTES, &id, &policy("13.6", &["a_cap"]))
                .unwrap_err(),
            AdmissionError::OsVersionTooLow
        );
        // Numeric, not lexicographic: 9 < 14 although "9" > "14" as text.
        assert_eq!(
            env.admit(&m, BYTES, &id, &policy("9.0", &["a_cap"]))
                .unwrap_err(),
            AdmissionError::OsVersionTooLow
        );
        env.admit(&m, BYTES, &id, &policy("14", &["a_cap"]))
            .unwrap();
        assert_eq!(
            env.admit(&m, BYTES, &id, &policy("15.0", &[])).unwrap_err(),
            AdmissionError::MissingCapability("a_cap".into())
        );
        let linux = CompatibilityPolicy::new(
            TargetOs::Linux,
            TargetArch::Aarch64,
            "99.0".into(),
            vec!["a_cap".into()],
        )
        .unwrap();
        assert_eq!(
            env.admit(&m, BYTES, &id, &linux).unwrap_err(),
            AdmissionError::PlatformMismatch
        );
        assert!(
            CompatibilityPolicy::new(TargetOs::Macos, TargetArch::Aarch64, "x".into(), vec![])
                .is_err()
        );
    }
}
