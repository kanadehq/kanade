//! Pure artifact naming and integrity contracts. These types do not authorize
//! publication or deployment, inspect packages (except the explicit legacy
//! probe), or establish publisher authenticity. Operator OS is never consulted.

use base64::{Engine, engine::general_purpose::URL_SAFE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error(transparent)]
    Identity(#[from] ArtifactIdentityError),
    #[error(transparent)]
    Key(#[from] AppPackageKeyError),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactIdentityError {
    #[error("unknown {component} {value:?}; expected {expected}")]
    UnknownComponent {
        component: &'static str,
        value: String,
        expected: &'static str,
    },
    #[error("invalid canonical SemVer {0:?} (maximum 128 bytes)")]
    InvalidVersion(String),
    #[error("Client Linux aarch64 is not supported")]
    ClientLinuxAarch64Unsupported,
    #[error(
        "unsupported artifact combination: role={role:?}, os={os:?}, arch={arch:?}, distro_abi={distro_abi:?}, format={format:?}"
    )]
    UnsupportedCombination {
        role: ArtifactRole,
        os: TargetOs,
        arch: TargetArch,
        distro_abi: DistroAbi,
        format: PackageFormat,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppPackageKeyError {
    #[error("expected canonical app key <role>/<version>/<os>-<arch>-<distro_abi>.<format>")]
    InvalidGrammar,
    #[error("Agent uses agent_releases and its existing release_key contract")]
    AgentRoleNotAllowed,
    #[error("noncanonical app package key")]
    NonCanonical,
}

fn invalid(message: impl Into<String>) -> ArtifactError {
    ArtifactError::Invalid(message.into())
}

macro_rules! tokens {
    ($name:ident { $($variant:ident => $token:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $token)] $variant),+ }
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $token),+ }
            }
        }
        impl FromStr for $name {
            type Err = ArtifactError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s { $($token => Ok(Self::$variant),)+
                    _ => Err(ArtifactIdentityError::UnknownComponent {
                        component: stringify!($name), value: s.to_owned(),
                        expected: concat!($($token, " "),+),
                    }.into()) }
            }
        }
    };
}

tokens!(ArtifactRole { Backend => "backend", Client => "client", Cli => "cli", Agent => "agent" });
tokens!(TargetOs { Windows => "windows", Linux => "linux", Macos => "macos" });
tokens!(TargetArch { X86_64 => "x86_64", Aarch64 => "aarch64" });
tokens!(DistroAbi { Msvc => "msvc", Musl => "musl", Ubuntu2404 => "ubuntu-24.04", Darwin => "darwin", Darwin14 => "darwin14" });
tokens!(PackageFormat { Exe => "exe", Deb => "deb", Pkg => "pkg", TarGz => "tar_gz", Zip => "zip" });

/// Canonical SemVer 2.0 text, including prerelease and build metadata.
/// Uses `semver` for validation; storage rejects aliases. Core numbers are
/// bounded by u64. Maximum serialized length: 128 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtifactVersion(String);

impl ArtifactVersion {
    /// Operator input only: trim whitespace and one leading `v`, consistently
    /// with agent publication. Never use this normalization for stored keys.
    pub fn from_input(s: &str) -> Result<Self, ArtifactError> {
        Self::from_str(crate::agent_metadata::normalize_version(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ArtifactVersion {
    type Err = ArtifactError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() > 128 || semver::Version::parse(s).is_err() {
            return Err(ArtifactIdentityError::InvalidVersion(s.to_owned()).into());
        }
        Ok(Self(s.to_owned()))
    }
}
impl TryFrom<String> for ArtifactVersion {
    type Error = ArtifactError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}
impl From<ArtifactVersion> for String {
    fn from(v: ArtifactVersion) -> Self {
        v.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "IdentityFields", into = "IdentityFields")]
pub struct ArtifactIdentity(IdentityFields);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityFields {
    role: ArtifactRole,
    version: ArtifactVersion,
    os: TargetOs,
    arch: TargetArch,
    distro_abi: DistroAbi,
    format: PackageFormat,
}

impl TryFrom<IdentityFields> for ArtifactIdentity {
    type Error = ArtifactError;
    fn try_from(f: IdentityFields) -> Result<Self, Self::Error> {
        use ArtifactRole::*;
        use DistroAbi::*;
        use PackageFormat::*;
        use TargetArch::*;
        use TargetOs::*;
        let allowed = matches!(
            (f.role, f.os, f.arch, f.distro_abi, f.format),
            (_, Windows, X86_64, Msvc, Exe | Zip)
                | (Client, Linux, X86_64, Ubuntu2404, Deb)
                | (Client, Macos, Aarch64, Darwin14, Pkg)
                | (Backend | Cli | Agent, Linux, X86_64 | Aarch64, Musl, TarGz)
                | (Cli | Agent, Macos, Aarch64, Darwin, TarGz)
        );
        if !allowed {
            if (f.role, f.os, f.arch) == (Client, Linux, Aarch64) {
                return Err(ArtifactIdentityError::ClientLinuxAarch64Unsupported.into());
            }
            return Err(ArtifactIdentityError::UnsupportedCombination {
                role: f.role,
                os: f.os,
                arch: f.arch,
                distro_abi: f.distro_abi,
                format: f.format,
            }
            .into());
        }
        Ok(Self(f))
    }
}
impl From<ArtifactIdentity> for IdentityFields {
    fn from(id: ArtifactIdentity) -> Self {
        id.0
    }
}

impl ArtifactIdentity {
    pub fn new(
        role: ArtifactRole,
        version: ArtifactVersion,
        os: TargetOs,
        arch: TargetArch,
        distro_abi: DistroAbi,
        format: PackageFormat,
    ) -> Result<Self, ArtifactError> {
        IdentityFields {
            role,
            version,
            os,
            arch,
            distro_abi,
            format,
        }
        .try_into()
    }
    pub fn role(&self) -> ArtifactRole {
        self.0.role
    }
    pub fn version(&self) -> &ArtifactVersion {
        &self.0.version
    }
    pub fn os(&self) -> TargetOs {
        self.0.os
    }
    pub fn arch(&self) -> TargetArch {
        self.0.arch
    }
    pub fn distro_abi(&self) -> DistroAbi {
        self.0.distro_abi
    }
    pub fn format(&self) -> PackageFormat {
        self.0.format
    }

    /// Object key only; bucket is `kv::OBJECT_APP_PACKAGES`. Never pass this
    /// key to the current two-segment legacy HTTP endpoints.
    pub fn app_package_key(&self) -> Result<String, ArtifactError> {
        if self.role() == ArtifactRole::Agent {
            return Err(AppPackageKeyError::AgentRoleNotAllowed.into());
        }
        Ok(format!(
            "{}/{}/{}-{}-{}.{}",
            self.role().as_str(),
            self.version().as_str(),
            self.os().as_str(),
            self.arch().as_str(),
            self.distro_abi().as_str(),
            self.format().as_str()
        ))
    }

    /// Strict inverse. No URL decoding, case folding, aliasing or path cleanup.
    pub fn from_app_package_key(key: &str) -> Result<Self, ArtifactError> {
        let bad = || ArtifactError::from(AppPackageKeyError::InvalidGrammar);
        let parts: Vec<_> = key.split('/').collect();
        if parts.len() != 3 {
            return Err(bad());
        }
        let (platform, format) = parts[2].rsplit_once('.').ok_or_else(bad)?;
        let mut platform = platform.splitn(3, '-');
        let os = platform.next().ok_or_else(bad)?.parse()?;
        let arch = platform.next().ok_or_else(bad)?.parse()?;
        let abi = platform.next().ok_or_else(bad)?.parse()?;
        let identity = Self::new(
            parts[0].parse()?,
            parts[1].parse()?,
            os,
            arch,
            abi,
            format.parse()?,
        )?;
        if identity.app_package_key()? != key {
            return Err(AppPackageKeyError::NonCanonical.into());
        }
        Ok(identity)
    }
}

/// Integrity only, never a publisher credential. JSON uses lowercase hex;
/// Object Store uses padded URL-safe base64 with the `SHA-256=` prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtifactSha256([u8; 32]);
impl ArtifactSha256 {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
    pub fn to_object_store_digest(self) -> String {
        format!("SHA-256={}", URL_SAFE.encode(self.0))
    }
    pub fn from_object_store_digest(s: &str) -> Result<Self, ArtifactError> {
        let raw = s
            .strip_prefix("SHA-256=")
            .ok_or_else(|| invalid("expected SHA-256= digest"))?;
        let bytes = URL_SAFE
            .decode(raw)
            .map_err(|_| invalid("invalid padded URL-safe SHA-256 digest"))?;
        let hash = Self(
            bytes
                .try_into()
                .map_err(|_| invalid("SHA-256 digest must be 32 bytes"))?,
        );
        if hash.to_object_store_digest() != s {
            return Err(invalid("noncanonical Object Store digest"));
        }
        Ok(hash)
    }
}
impl From<ArtifactSha256> for String {
    fn from(hash: ArtifactSha256) -> String {
        hash.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}
impl TryFrom<String> for ArtifactSha256 {
    type Error = ArtifactError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("SHA-256 must be exactly 64 lowercase hex digits"));
        }
        let mut bytes = [0; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| invalid("invalid SHA-256"))?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIntegrity {
    pub sha256: ArtifactSha256,
    pub size_bytes: u64,
}
impl ArtifactIntegrity {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: ArtifactSha256::of(bytes),
            size_bytes: bytes.len() as u64,
        }
    }
}

/// A legacy PE observation, NOT an alias, admitted package, or trusted release.
/// Role is caller-declared and not proven by PE headers. Missing VERSIONINFO
/// stays explicit; downstream migration must verify version, role, publisher
/// evidence and snapshot the bytes before permitting deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyWindowsCandidate {
    pub identity: ArtifactIdentity,
    pub integrity: ArtifactIntegrity,
    pub embedded_version_verified: bool,
}

pub fn inspect_legacy_windows(
    role: ArtifactRole,
    declared_version: &str,
    bytes: &[u8],
) -> Result<LegacyWindowsCandidate, ArtifactError> {
    use object::Object;
    if crate::bin_platform::AgentPlatform::detect(bytes).map_err(invalid)?
        != crate::bin_platform::AgentPlatform::WindowsX86_64
    {
        return Err(invalid(
            "legacy Windows candidate requires PE/COFF AMD64 (0x8664)",
        ));
    }
    let file =
        object::File::parse(bytes).map_err(|e| invalid(format!("invalid legacy PE: {e}")))?;
    if file.format() != object::BinaryFormat::Pe || file.kind() != object::ObjectKind::Executable {
        return Err(invalid(
            "legacy Windows candidate must be a PE executable, not a DLL or object",
        ));
    }
    let version = ArtifactVersion::from_input(declared_version)?;
    let embedded = crate::exe_version::extract_pe_version(bytes);
    if let Some(ref embedded) = embedded {
        let observed = ArtifactVersion::from_input(embedded)?;
        if observed != version {
            return Err(invalid(format!(
                "legacy PE version mismatch: declared {}, embedded {}",
                version.as_str(),
                observed.as_str()
            )));
        }
    }
    Ok(LegacyWindowsCandidate {
        identity: ArtifactIdentity::new(
            role,
            version,
            TargetOs::Windows,
            TargetArch::X86_64,
            DistroAbi::Msvc,
            PackageFormat::Exe,
        )?,
        integrity: ArtifactIntegrity::of(bytes),
        embedded_version_verified: embedded.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn identity(
        role: ArtifactRole,
        os: TargetOs,
        arch: TargetArch,
        abi: DistroAbi,
        format: PackageFormat,
    ) -> Result<ArtifactIdentity, ArtifactError> {
        ArtifactIdentity::new(
            role,
            "1.2.3-rc.1+build.007".parse().unwrap(),
            os,
            arch,
            abi,
            format,
        )
    }

    #[test]
    fn complete_support_matrix_round_trips_without_collisions() {
        use ArtifactRole::*;
        use DistroAbi::*;
        use PackageFormat::*;
        use TargetArch::*;
        use TargetOs::*;
        let mut keys = HashSet::new();
        let mut accepted = 0;
        for role in [Backend, Client, Cli, Agent] {
            for os in [Windows, Linux, Macos] {
                for arch in [X86_64, Aarch64] {
                    for abi in [Msvc, Musl, Ubuntu2404, Darwin, Darwin14] {
                        for format in [Exe, Deb, Pkg, TarGz, Zip] {
                            let expected = match role {
                                Client => matches!(
                                    (os, arch, abi, format),
                                    (Windows, X86_64, Msvc, Exe | Zip)
                                        | (Linux, X86_64, Ubuntu2404, Deb)
                                        | (Macos, Aarch64, Darwin14, Pkg)
                                ),
                                Backend => matches!(
                                    (os, arch, abi, format),
                                    (Windows, X86_64, Msvc, Exe | Zip)
                                        | (Linux, X86_64 | Aarch64, Musl, TarGz)
                                ),
                                Cli | Agent => matches!(
                                    (os, arch, abi, format),
                                    (Windows, X86_64, Msvc, Exe | Zip)
                                        | (Linux, X86_64 | Aarch64, Musl, TarGz)
                                        | (Macos, Aarch64, Darwin, TarGz)
                                ),
                            };
                            let result = identity(role, os, arch, abi, format);
                            assert_eq!(
                                result.is_ok(),
                                expected,
                                "{role:?}/{os:?}/{arch:?}/{abi:?}/{format:?}"
                            );
                            if let Ok(id) = result {
                                accepted += 1;
                                let json = serde_json::to_string(&id).unwrap();
                                assert_eq!(
                                    serde_json::from_str::<ArtifactIdentity>(&json).unwrap(),
                                    id
                                );
                                if role == Agent {
                                    assert!(id.app_package_key().is_err());
                                } else {
                                    let key = id.app_package_key().unwrap();
                                    assert_eq!(
                                        ArtifactIdentity::from_app_package_key(&key).unwrap(),
                                        id
                                    );
                                    assert!(keys.insert(key));
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(accepted, 18);
        assert_eq!(keys.len(), 13);
    }

    #[test]
    fn errors_distinguish_identity_from_key_failures() {
        assert!(matches!(
            identity(
                ArtifactRole::Client,
                TargetOs::Linux,
                TargetArch::Aarch64,
                DistroAbi::Ubuntu2404,
                PackageFormat::Deb
            ),
            Err(ArtifactError::Identity(
                ArtifactIdentityError::ClientLinuxAarch64Unsupported
            ))
        ));
        assert!(matches!(
            "01.2.3".parse::<ArtifactVersion>(),
            Err(ArtifactError::Identity(
                ArtifactIdentityError::InvalidVersion(_)
            ))
        ));
        assert!(matches!(
            ArtifactIdentity::from_app_package_key("../bad"),
            Err(ArtifactError::Key(AppPackageKeyError::InvalidGrammar))
        ));
        assert!(matches!(
            "amd64".parse::<TargetArch>(),
            Err(ArtifactError::Identity(
                ArtifactIdentityError::UnknownComponent { .. }
            ))
        ));
        assert!(matches!(
            identity(
                ArtifactRole::Backend,
                TargetOs::Macos,
                TargetArch::Aarch64,
                DistroAbi::Darwin,
                PackageFormat::TarGz
            ),
            Err(ArtifactError::Identity(
                ArtifactIdentityError::UnsupportedCombination { .. }
            ))
        ));
        let agent = identity(
            ArtifactRole::Agent,
            TargetOs::Windows,
            TargetArch::X86_64,
            DistroAbi::Msvc,
            PackageFormat::Exe,
        )
        .unwrap();
        assert!(matches!(
            agent.app_package_key(),
            Err(ArtifactError::Key(AppPackageKeyError::AgentRoleNotAllowed))
        ));
    }

    #[test]
    fn canonical_json_and_keys_are_fixed_and_preserve_version_metadata() {
        let json = r#"{"role":"client","version":"1.2.3-rc.1+build.007","os":"linux","arch":"x86_64","distro_abi":"ubuntu-24.04","format":"deb"}"#;
        let id: ArtifactIdentity = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), json);
        assert_eq!(
            id.app_package_key().unwrap(),
            "client/1.2.3-rc.1+build.007/linux-x86_64-ubuntu-24.04.deb"
        );
        let mut version_keys = HashSet::new();
        for version in ["1.2.3", "1.2.3-rc.1", "1.2.3+build.A", "1.2.3+build.a"] {
            let changed = json.replace("1.2.3-rc.1+build.007", version);
            let id: ArtifactIdentity = serde_json::from_str(&changed).unwrap();
            let key = id.app_package_key().unwrap();
            assert_eq!(ArtifactIdentity::from_app_package_key(&key).unwrap(), id);
            assert!(version_keys.insert(key));
        }
        let tar = "backend/1.2.3/linux-aarch64-musl.tar_gz";
        assert_eq!(
            ArtifactIdentity::from_app_package_key(tar)
                .unwrap()
                .app_package_key()
                .unwrap(),
            tar
        );
        for altered in [
            json.replace("ubuntu-24.04", "musl"),
            json.replace("x86_64", "aarch64"),
            json.replace("\"ubuntu-24.04\"", "null"),
            json.replace("\"format\":", "\"extra\":0,\"format\":"),
            json.replace("1.2.3-rc.1+build.007", "v1.2.3"),
            json.replace("deb", "rpm"),
        ] {
            assert!(
                serde_json::from_str::<ArtifactIdentity>(&altered).is_err(),
                "{altered}"
            );
        }
    }

    #[test]
    fn strict_versions_and_input_normalization_are_separate() {
        assert_eq!(
            ArtifactVersion::from_input(" v1.2.3+build.01 \n")
                .unwrap()
                .as_str(),
            "1.2.3+build.01"
        );
        for good in ["0.0.0", "1.2.3-alpha-1.0+abc.001", "1.2.3+other", "1.2.3-A"] {
            assert_eq!(good.parse::<ArtifactVersion>().unwrap().as_str(), good);
        }
        for bad in [
            "",
            "1",
            "1.2",
            "01.2.3",
            "1.2.3-01",
            "1.2.3-",
            "1.2.3+",
            "1.2.3-a..b",
            "1.2.3+a+b",
            "1.2.3/a",
            "1.2.3\\a",
            "1.2.3%2f",
            "1.2.3\n",
            "v1.2.3",
            "1.2.3+日本語",
            "18446744073709551616.0.0",
        ] {
            assert!(bad.parse::<ArtifactVersion>().is_err(), "{bad:?}");
        }
        assert!(
            format!("1.2.3+{}", "a".repeat(128))
                .parse::<ArtifactVersion>()
                .is_err()
        );
    }

    #[test]
    fn keys_reject_traversal_and_normalization_aliases() {
        for key in [
            "app_packages/client/1.2.3/windows-x86_64-msvc.exe",
            "/client/1.2.3/windows-x86_64-msvc.exe",
            "client//1.2.3/windows-x86_64-msvc.exe",
            "client/../windows-x86_64-msvc.exe",
            "client/1.2.3/../windows-x86_64-msvc.exe",
            "client/1.2.3/windows-x86_64-msvc.exe/",
            "Client/1.2.3/windows-x86_64-msvc.exe",
            "client/v1.2.3/windows-x86_64-msvc.exe",
            "client/1.2.3/windows-amd64-msvc.exe",
            "client/1.2.3/windows-x86_64.exe",
            "client/1.2.3/windows-x86_64-msvc.EXE",
            "client/1.2.3/windows-x86_64-msvc.exe%2f..",
            "client/1.2.3/windows-x86_64-msvc.exe\\..",
            "client/1.2.3/windows-x86_64-msvc.exe\0",
            "backend/1.2.3/linux-x86_64-musl.tar.gz",
            "client/1.2.3/linux-aarch64-ubuntu-24.04.deb",
            "backend/1.2.3/macos-aarch64-darwin.tar_gz",
            "agent/1.2.3/windows-x86_64-msvc.exe",
        ] {
            assert!(
                ArtifactIdentity::from_app_package_key(key).is_err(),
                "{key:?}"
            );
        }
    }

    #[test]
    fn integrity_json_and_object_store_conversion_are_exact() {
        let integrity = ArtifactIntegrity::of(b"");
        let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(String::from(integrity.sha256), hex);
        assert_eq!(
            serde_json::to_string(&integrity).unwrap(),
            format!(r#"{{"sha256":"{hex}","size_bytes":0}}"#)
        );
        let nats = integrity.sha256.to_object_store_digest();
        assert_eq!(nats, "SHA-256=47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU=");
        assert_eq!(
            ArtifactSha256::from_object_store_digest(&nats).unwrap(),
            integrity.sha256
        );
        for bad in [hex.to_uppercase(), "0".repeat(63), "g".repeat(64)] {
            assert!(ArtifactSha256::try_from(bad).is_err());
        }
        for bad in [
            nats.trim_end_matches('=').to_owned(),
            nats.replace("SHA-256", "sha256"),
            "SHA-256=AA==".into(),
            hex.into(),
        ] {
            assert!(ArtifactSha256::from_object_store_digest(&bad).is_err());
        }
    }

    #[test]
    fn integrity_metadata_deserializes_canonical_hex_and_size() {
        for (bytes, hex) in [
            (
                &b""[..],
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                &b"abc"[..],
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        ] {
            let json = format!(r#"{{"sha256":"{hex}","size_bytes":{}}}"#, bytes.len());
            let restored: ArtifactIntegrity = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, ArtifactIntegrity::of(bytes));
            assert_eq!(serde_json::to_string(&restored).unwrap(), json);
            assert_eq!(
                ArtifactSha256::try_from(hex.to_owned()).unwrap(),
                restored.sha256
            );
        }
    }

    // A real VERSIONINFO resource tree in a synthetic PE. No execution or
    // checked-in binary fixture is needed to exercise the production extractor.
    fn pe_with_version(version: &str) -> Vec<u8> {
        fn wide(s: &str) -> Vec<u8> {
            s.encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect()
        }
        fn node(key: &str, value: &[u8], text: bool, children: &[Vec<u8>]) -> Vec<u8> {
            let mut out = vec![0; 6];
            out[2..4].copy_from_slice(
                &((if text { value.len() / 2 } else { value.len() }) as u16).to_le_bytes(),
            );
            out[4..6].copy_from_slice(&u16::from(text).to_le_bytes());
            out.extend(wide(key));
            while out.len() % 4 != 0 {
                out.push(0);
            }
            out.extend(value);
            for child in children {
                while out.len() % 4 != 0 {
                    out.push(0);
                }
                out.extend(child);
            }
            let len = out.len() as u16;
            out[..2].copy_from_slice(&len.to_le_bytes());
            out
        }
        let string = node("ProductVersion", &wide(version), true, &[]);
        let table = node("040904b0", &[], true, &[string]);
        let strings = node("StringFileInfo", &[], true, &[table]);
        let translation = node("Translation", &[0x09, 0x04, 0xb0, 0x04], false, &[]);
        let vars = node("VarFileInfo", &[], true, &[translation]);
        let mut fixed = [0; 52];
        fixed[..4].copy_from_slice(&0xfeef04bdu32.to_le_bytes());
        let version_info = node("VS_VERSION_INFO", &fixed, false, &[strings, vars]);
        let mut resource = vec![0; 88];
        for (offset, name, target) in [
            (0, 16u32, 0x80000018u32),
            (24, 1, 0x80000030),
            (48, 0x409, 72),
        ] {
            resource[offset + 14..offset + 16].copy_from_slice(&1u16.to_le_bytes());
            resource[offset + 16..offset + 20].copy_from_slice(&name.to_le_bytes());
            resource[offset + 20..offset + 24].copy_from_slice(&target.to_le_bytes());
        }
        resource[72..76].copy_from_slice(&0x1058u32.to_le_bytes());
        resource[76..80].copy_from_slice(&(version_info.len() as u32).to_le_bytes());
        resource.extend(version_info);
        let mut pe = crate::agent_metadata::fixtures::pe_x86_64(".rsrc", &resource);
        pe[0x98 + 108..0x98 + 112].copy_from_slice(&16u32.to_le_bytes());
        pe[0x98 + 128..0x98 + 132].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[0x98 + 132..0x98 + 136].copy_from_slice(&(resource.len() as u32).to_le_bytes());
        pe
    }

    #[test]
    fn legacy_embedded_version_is_checked_without_proving_role_or_publisher() {
        let pe = pe_with_version("1.2.3");
        assert_eq!(
            crate::exe_version::extract_pe_version(&pe).as_deref(),
            Some("1.2.3")
        );
        let candidate = inspect_legacy_windows(ArtifactRole::Cli, "v1.2.3", &pe).unwrap();
        assert!(candidate.embedded_version_verified);
        let err = inspect_legacy_windows(ArtifactRole::Cli, "1.2.4", &pe).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
        let prerelease = "1.2.3-rc.1+build.42";
        let pe = pe_with_version(prerelease);
        let candidate = inspect_legacy_windows(ArtifactRole::Cli, prerelease, &pe).unwrap();
        assert!(candidate.embedded_version_verified);
        assert_eq!(candidate.identity.version().as_str(), prerelease);
        let err =
            inspect_legacy_windows(ArtifactRole::Cli, "1.2.3-rc.1+build.43", &pe).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
        let bad_version = pe_with_version("1.2");
        assert!(inspect_legacy_windows(ArtifactRole::Cli, "1.2.3", &bad_version).is_err());
    }

    #[test]
    fn legacy_probe_requires_real_amd64_pe_and_leaves_missing_evidence_explicit() {
        let pe = crate::agent_metadata::fixtures::pe_x86_64(".test", b"fixture");
        let candidate = inspect_legacy_windows(ArtifactRole::Client, " v1.2.3 ", &pe).unwrap();
        assert!(!candidate.embedded_version_verified);
        assert_eq!(
            candidate.identity.app_package_key().unwrap(),
            "client/1.2.3/windows-x86_64-msvc.exe"
        );
        assert_eq!(candidate.integrity, ArtifactIntegrity::of(&pe));
        let mut arm = pe.clone();
        arm[0x84..0x86].copy_from_slice(&0xaa64u16.to_le_bytes());
        let mut dll = pe.clone();
        dll[0x96..0x98].copy_from_slice(&0x2022u16.to_le_bytes());
        for bad in [
            &b"kanade-client/1.2.3"[..],
            &b"MZ"[..],
            &arm,
            &dll,
            &pe[..0x90],
        ] {
            assert!(inspect_legacy_windows(ArtifactRole::Client, "1.2.3", bad).is_err());
        }
        assert!(inspect_legacy_windows(ArtifactRole::Client, "2025.03", &pe).is_err());
    }
}
