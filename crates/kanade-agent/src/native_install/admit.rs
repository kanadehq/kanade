//! Turning raw inputs into the only thing the installer will accept: bytes
//! that were independently re-hashed, bound to the typed operation, admitted
//! against the *current* managed publisher trust, and structurally proven to
//! be the expected arm64 CLI.
//!
//! Approval is deliberately not produced here: admission says the package is
//! genuine; the operator's approval (see [`super::model::Approval`]) says it
//! may be deployed to this host. Neither implies the other.

use kanade_shared::artifact::{
    ArtifactIntegrity, ArtifactRole, ArtifactSha256, DistroAbi, PackageFormat, TargetArch, TargetOs,
};
use kanade_shared::package_admission::{
    AdmissionRequest, AdmittedPackage, CompatibilityPolicy, admit_package,
};
use kanade_shared::publisher_trust::PublisherTrustStore;
use kanade_shared::typed_operation::{DeployCliSpec, TypedOperation};

use super::archive::{self, MAX_ARCHIVE_BYTES};
use super::macho;
use super::model::{HostInfo, InstallError};

/// Raw, untrusted inputs of one install request.
pub struct InstallInputs<'a> {
    pub operation: &'a TypedOperation,
    pub manifest_bytes: &'a [u8],
    pub artifact_bytes: &'a [u8],
    pub host: &'a HostInfo,
}

/// Verified evidence for one install: no public constructor.
pub struct VerifiedPackage {
    pub(super) spec: DeployCliSpec,
    pub(super) admitted: AdmittedPackage,
    pub(super) binary: Vec<u8>,
    pub(super) binary_sha256: ArtifactSha256,
}

impl VerifiedPackage {
    pub fn version(&self) -> &str {
        self.spec.artifact_identity.version().as_str()
    }
    pub fn binary_sha256(&self) -> &ArtifactSha256 {
        &self.binary_sha256
    }
}

fn host_major(os_version: &str) -> Option<u64> {
    os_version.split('.').next()?.parse().ok()
}

/// Verify `inputs` against `store`'s current trust. Pure with respect to the
/// filesystem: it reads nothing and writes nothing.
pub fn verify_inputs(
    store: &PublisherTrustStore,
    inputs: &InstallInputs<'_>,
) -> Result<VerifiedPackage, InstallError> {
    let TypedOperation::DeployCli(spec) = inputs.operation else {
        return Err(InstallError::Unsupported(
            "only deploy_cli is implemented by this handler".into(),
        ));
    };
    inputs
        .operation
        .validate()
        .map_err(InstallError::Admission)?;
    let id = &spec.artifact_identity;
    if (id.role(), id.os(), id.arch(), id.distro_abi(), id.format())
        != (
            ArtifactRole::Cli,
            TargetOs::Macos,
            TargetArch::Aarch64,
            DistroAbi::Darwin,
            PackageFormat::TarGz,
        )
    {
        return Err(InstallError::Unsupported(
            "this handler installs only the macOS arm64 admin CLI tar.gz".into(),
        ));
    }
    if host_major(&inputs.host.os_version).is_none_or(|m| m < 14) {
        return Err(InstallError::Unsupported(
            "macOS 14 or newer is required".into(),
        ));
    }
    if spec.artifact_size_bytes > MAX_ARCHIVE_BYTES {
        return Err(InstallError::Admission(
            "artifact_size_bytes exceeds the archive limit".into(),
        ));
    }
    // The expectation comes from the operation (itself bound to the approved
    // plan), never from the manifest or the archive being checked.
    let expected_integrity = ArtifactIntegrity {
        sha256: spec.artifact_sha256,
        size_bytes: spec.artifact_size_bytes,
    };
    let policy = CompatibilityPolicy::new(
        TargetOs::Macos,
        TargetArch::Aarch64,
        inputs.host.os_version.clone(),
        inputs.host.capabilities.clone(),
    )
    .map_err(|e| InstallError::Admission(e.to_string()))?;
    let admitted = admit_package(
        store,
        &AdmissionRequest {
            manifest_bytes: inputs.manifest_bytes,
            artifact_bytes: inputs.artifact_bytes,
            expected_identity: id,
            expected_integrity: &expected_integrity,
            expected_package_id: &spec.package_id,
            policy: &policy,
        },
    )
    .map_err(|e| InstallError::Admission(e.to_string()))?;
    inputs
        .operation
        .check_admission(&admitted)
        .map_err(InstallError::Admission)?;

    let binary = archive::extract_cli_member(inputs.artifact_bytes)?;
    macho::check_arm64_executable(&binary)?;
    let binary_sha256 = ArtifactSha256::of(&binary);
    Ok(VerifiedPackage {
        spec: spec.clone(),
        admitted,
        binary,
        binary_sha256,
    })
}
