//! Release metadata embedded in official `kanade-agent` binaries, and the
//! admission check `POST /api/agents/publish` runs against an upload.
//!
//! The agent build stamps a dedicated section into the executable —
//! `.kanade` in a Windows PE / Linux ELF, `__DATA,__kanade` in a macOS
//! Mach-O — holding a small envelope:
//!
//! ```text
//! offset 0   8 bytes  magic "KNAGMETA"
//! offset 8   u16 LE   schema version (1)
//! offset 10  u16 LE   flags (0)
//! offset 12  u32 LE   payload length
//! offset 16  payload  JSON {"product","version","platform"}
//! ```
//!
//! Admission only *reads* the file (the `object` crate walks the container
//! structure; nothing is executed), so it is safe for foreign-platform
//! artifacts. It identifies the product the build system declared; it is
//! not a signature and does not authenticate a deliberately forged binary.
//! A binary without the section (built before this metadata existed) is
//! rejected — see `book/src/operations/agent-release-admission.md`.

use object::read::{Object, ObjectSection};
use serde::{Deserialize, Serialize};

use crate::bin_platform::{self, AgentPlatform};

/// Section name in PE / ELF images.
pub const SECTION_PE_ELF: &str = ".kanade";
/// Mach-O segment and section holding the metadata.
pub const SECTION_MACHO_SEGMENT: &str = "__DATA";
pub const SECTION_MACHO_NAME: &str = "__kanade";

pub const MAGIC: &[u8; 8] = b"KNAGMETA";
pub const SCHEMA_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 16;
/// Product identifier the official agent build embeds.
pub const AGENT_PRODUCT_NAME: &str = "kanade-agent";

/// Payload of the metadata section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBinaryMetadata {
    pub product: String,
    pub version: String,
    /// `AgentPlatform::as_str()` of the build target, e.g. `linux-x86_64`.
    pub platform: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    #[error("{0}")]
    UnsupportedBinary(String),
    #[error(
        "the binary carries no kanade-agent release metadata section ({0}); it is not an official \
         metadata-bearing kanade-agent build (a legacy agent, the `kanade` CLI, the backend, or \
         another file). Publish a kanade-agent built from a release that embeds the metadata \
         (the first release after v0.80.0)"
    )]
    MissingMetadata(&'static str),
    #[error("the binary has more than one agent release metadata section; refusing it")]
    DuplicateSection,
    #[error("malformed agent release metadata: {0}")]
    Malformed(String),
    #[error(
        "unsupported agent release metadata schema version {0} (this backend understands \
         {SCHEMA_VERSION}); upgrade the backend before publishing this agent"
    )]
    UnsupportedSchema(u16),
    #[error(
        "the binary identifies itself as product '{0}', not '{AGENT_PRODUCT_NAME}'; only the \
         kanade-agent binary may be published as an agent release"
    )]
    WrongProduct(String),
    #[error(
        "the file is a {container} executable but its embedded metadata declares platform \
         '{embedded}'"
    )]
    PlatformMismatch {
        container: &'static str,
        embedded: String,
    },
    #[error(
        "version mismatch: the binary embeds version '{embedded}' but '{declared}' was \
         requested; omit the version to use the embedded one or publish the matching build"
    )]
    VersionMismatch { declared: String, embedded: String },
    #[error("invalid embedded version: {0}")]
    InvalidVersion(String),
}

/// What an upload turned out to be, once admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedAgent {
    pub platform: AgentPlatform,
    /// Normalised embedded version (no `v`, no whitespace).
    pub version: String,
    /// `agent_releases` Object Store key.
    pub key: String,
}

/// Trim whitespace and one leading `v`.
pub fn normalize_version(v: &str) -> &str {
    let t = v.trim();
    t.strip_prefix('v').unwrap_or(t)
}

/// Serialise the envelope the agent build embeds.
pub fn encode_metadata_bytes(meta: &AgentBinaryMetadata) -> Vec<u8> {
    let json = serde_json::to_vec(meta).expect("serialize agent metadata");
    let mut out = Vec::with_capacity(HEADER_LEN + json.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out
}

/// Parse the section contents. Zero padding after the payload is tolerated
/// (PE file alignment); anything else trailing is malformed.
pub fn parse_metadata_bytes(raw: &[u8]) -> Result<AgentBinaryMetadata, AdmissionError> {
    let bad = |m: &str| AdmissionError::Malformed(m.to_string());
    if raw.len() < HEADER_LEN {
        return Err(bad("section shorter than the envelope header"));
    }
    if &raw[..8] != MAGIC {
        return Err(bad("bad magic (expected KNAGMETA)"));
    }
    let schema = u16::from_le_bytes([raw[8], raw[9]]);
    if schema != SCHEMA_VERSION {
        return Err(AdmissionError::UnsupportedSchema(schema));
    }
    if u16::from_le_bytes([raw[10], raw[11]]) != 0 {
        return Err(bad("unknown flags"));
    }
    let len = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
    let end = HEADER_LEN
        .checked_add(len)
        .filter(|e| *e <= raw.len())
        .ok_or_else(|| bad("payload length exceeds the section"))?;
    if raw[end..].iter().any(|b| *b != 0) {
        return Err(bad("unexpected data after the payload"));
    }
    serde_json::from_slice(&raw[HEADER_LEN..end])
        .map_err(|e| AdmissionError::Malformed(format!("payload is not valid metadata JSON: {e}")))
}

/// Locate the metadata section by structure (never by scanning for bytes).
fn find_section(file: &object::File<'_>) -> Result<Option<Vec<u8>>, AdmissionError> {
    let macho = file.format() == object::BinaryFormat::MachO;
    let mut found: Option<Vec<u8>> = None;
    for sec in file.sections() {
        let hit = if macho {
            sec.segment_name().ok().flatten() == Some(SECTION_MACHO_SEGMENT)
                && sec.name().ok() == Some(SECTION_MACHO_NAME)
        } else {
            sec.name().ok() == Some(SECTION_PE_ELF)
        };
        if !hit {
            continue;
        }
        if found.is_some() {
            return Err(AdmissionError::DuplicateSection);
        }
        let data = sec
            .data()
            .map_err(|e| AdmissionError::Malformed(format!("section data out of range: {e}")))?;
        found = Some(data.to_vec());
    }
    Ok(found)
}

/// Admission check for an uploaded agent binary. Pure: no I/O, no
/// execution. `declared` is the caller's requested version, if any.
pub fn admit_agent_binary(
    bytes: &[u8],
    declared: Option<&str>,
) -> Result<AdmittedAgent, AdmissionError> {
    // OS / CPU from the container itself, never the filename.
    let platform = AgentPlatform::detect(bytes).map_err(AdmissionError::UnsupportedBinary)?;

    let file = object::File::parse(bytes).map_err(|e| {
        AdmissionError::UnsupportedBinary(format!("cannot parse the executable structure: {e}"))
    })?;
    let section_name = if file.format() == object::BinaryFormat::MachO {
        "__DATA,__kanade"
    } else {
        SECTION_PE_ELF
    };
    let raw = find_section(&file)?.ok_or(AdmissionError::MissingMetadata(section_name))?;
    let meta = parse_metadata_bytes(&raw)?;

    if meta.product != AGENT_PRODUCT_NAME {
        return Err(AdmissionError::WrongProduct(meta.product));
    }
    if meta.platform != platform.as_str() {
        return Err(AdmissionError::PlatformMismatch {
            container: platform.as_str(),
            embedded: meta.platform,
        });
    }

    let version = normalize_version(&meta.version).to_string();
    bin_platform::check_release_key(&version).map_err(AdmissionError::InvalidVersion)?;

    if let Some(d) = declared
        && normalize_version(d) != version
    {
        return Err(AdmissionError::VersionMismatch {
            declared: d.trim().to_string(),
            embedded: meta.version,
        });
    }

    // A Windows build also carries VERSIONINFO; a contradiction means the
    // two stamps came from different builds.
    if matches!(
        platform,
        AgentPlatform::WindowsX86_64 | AgentPlatform::WindowsAarch64
    ) && let Some(pe) = crate::exe_version::extract_pe_version(bytes)
        && normalize_version(&pe) != version
    {
        return Err(AdmissionError::VersionMismatch {
            declared: pe,
            embedded: meta.version,
        });
    }

    let key = platform.release_key(&version);
    bin_platform::check_release_key(&key).map_err(AdmissionError::InvalidVersion)?;
    Ok(AdmittedAgent {
        platform,
        version,
        key,
    })
}

/// Fixture builders shared by tests across crates.
#[cfg(any(test, feature = "test-util"))]
pub mod fixtures {
    use super::*;
    use object::write::Object;
    use object::{Architecture, BinaryFormat, Endianness, SectionKind};

    pub fn meta(product: &str, version: &str, platform: &str) -> Vec<u8> {
        encode_metadata_bytes(&AgentBinaryMetadata {
            product: product.into(),
            version: version.into(),
            platform: platform.into(),
        })
    }

    /// An ELF / Mach-O object-file executable stand-in with `sections`
    /// (segment, name, data). The container structure is real; the code is not.
    pub fn image(
        format: BinaryFormat,
        arch: Architecture,
        sections: &[(&str, &str, &[u8])],
    ) -> Vec<u8> {
        let mut obj = Object::new(format, arch, Endianness::Little);
        for (seg, name, data) in sections {
            let id = obj.add_section(
                seg.as_bytes().to_vec(),
                name.as_bytes().to_vec(),
                SectionKind::ReadOnlyData,
            );
            obj.append_section_data(id, data, 1);
        }
        obj.write().expect("write fixture")
    }

    pub fn elf_x86_64(sections: &[(&str, &str, &[u8])]) -> Vec<u8> {
        image(BinaryFormat::Elf, Architecture::X86_64, sections)
    }

    pub fn macho_arm64(sections: &[(&str, &str, &[u8])]) -> Vec<u8> {
        image(BinaryFormat::MachO, Architecture::Aarch64, sections)
    }

    /// Minimal PE32+ (amd64) with one section.
    pub fn pe_x86_64(section: &str, data: &[u8]) -> Vec<u8> {
        let mut pe = vec![0u8; 0x80];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe.extend_from_slice(b"PE\0\0");
        pe.extend_from_slice(&0x8664u16.to_le_bytes());
        pe.extend_from_slice(&1u16.to_le_bytes());
        pe.extend_from_slice(&[0u8; 12]);
        pe.extend_from_slice(&0xF0u16.to_le_bytes());
        pe.extend_from_slice(&0x0022u16.to_le_bytes());
        let opt = pe.len();
        pe.extend_from_slice(&0x020Bu16.to_le_bytes());
        pe.resize(opt + 0xF0, 0);
        let mut sh = [0u8; 40];
        sh[..section.len().min(8)].copy_from_slice(&section.as_bytes()[..section.len().min(8)]);
        sh[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        sh[12..16].copy_from_slice(&0x1000u32.to_le_bytes());
        sh[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
        sh[20..24].copy_from_slice(&0x200u32.to_le_bytes());
        sh[36..40].copy_from_slice(&0x4000_0040u32.to_le_bytes());
        pe.extend_from_slice(&sh);
        pe.resize(0x200, 0);
        pe.extend_from_slice(data);
        pe
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    const K: &str = ".kanade";

    fn elf(m: &[u8]) -> Vec<u8> {
        elf_x86_64(&[("", K, m)])
    }

    #[test]
    fn admits_each_supported_platform() {
        let a = admit_agent_binary(
            &elf(&meta("kanade-agent", "0.81.0", "linux-x86_64")),
            Some("0.81.0"),
        )
        .unwrap();
        assert_eq!(
            (a.platform, a.key.as_str()),
            (AgentPlatform::LinuxX86_64, "0.81.0-linux-x86_64")
        );

        let arm = image(
            object::BinaryFormat::Elf,
            object::Architecture::Aarch64,
            &[("", K, &meta("kanade-agent", "0.81.0", "linux-aarch64"))],
        );
        assert_eq!(
            admit_agent_binary(&arm, Some("v0.81.0")).unwrap().key,
            "0.81.0-linux-aarch64"
        );

        let mac = macho_arm64(&[(
            "__DATA",
            "__kanade",
            &meta("kanade-agent", "0.81.0", "macos-aarch64"),
        )]);
        assert_eq!(
            admit_agent_binary(&mac, None).unwrap().key,
            "0.81.0-macos-aarch64"
        );

        let pe = pe_x86_64(K, &meta("kanade-agent", "0.81.0", "windows-x86_64"));
        assert_eq!(
            admit_agent_binary(&pe, Some(" 0.81.0 ")).unwrap().key,
            "0.81.0"
        );
    }

    #[test]
    fn pe_file_alignment_padding_is_tolerated() {
        let mut m = meta("kanade-agent", "0.81.0", "windows-x86_64");
        m.extend_from_slice(&[0u8; 100]);
        assert!(admit_agent_binary(&pe_x86_64(K, &m), None).is_ok());
    }

    #[test]
    fn rejects_wrong_product() {
        for product in ["kanade", "kanade-backend", "kanade-client", ""] {
            let e = admit_agent_binary(&elf(&meta(product, "0.81.0", "linux-x86_64")), None)
                .unwrap_err();
            assert!(
                matches!(e, AdmissionError::WrongProduct(_)),
                "{product}: {e}"
            );
        }
        let mac = macho_arm64(&[(
            "__DATA",
            "__kanade",
            &meta("kanade", "0.79.0", "macos-aarch64"),
        )]);
        assert!(matches!(
            admit_agent_binary(&mac, Some("0.79.0")),
            Err(AdmissionError::WrongProduct(_))
        ));
    }

    #[test]
    fn rejects_version_mismatch() {
        let e = admit_agent_binary(
            &elf(&meta("kanade-agent", "0.79.0", "linux-x86_64")),
            Some("0.80.0"),
        )
        .unwrap_err();
        assert!(matches!(e, AdmissionError::VersionMismatch { .. }));
    }

    #[test]
    fn rejects_platform_mismatch() {
        let e = admit_agent_binary(
            &elf(&meta("kanade-agent", "0.81.0", "windows-x86_64")),
            None,
        )
        .unwrap_err();
        assert!(matches!(e, AdmissionError::PlatformMismatch { .. }));
    }

    #[test]
    fn rejects_legacy_binary_without_section() {
        let e = admit_agent_binary(
            &elf_x86_64(&[("", ".rodata", b"kanade-agent 0.81.0")]),
            Some("0.81.0"),
        )
        .unwrap_err();
        assert!(matches!(e, AdmissionError::MissingMetadata(_)));
        // The same section name in the wrong Mach-O segment does not count.
        let mac = macho_arm64(&[(
            "__TEXT",
            "__kanade",
            &meta("kanade-agent", "0.81.0", "macos-aarch64"),
        )]);
        assert!(matches!(
            admit_agent_binary(&mac, None),
            Err(AdmissionError::MissingMetadata(_))
        ));
    }

    #[test]
    fn identifying_bytes_outside_the_section_are_ignored() {
        let mut bin = elf_x86_64(&[("", ".rodata", b"x")]);
        bin.extend_from_slice(&meta("kanade-agent", "0.81.0", "linux-x86_64"));
        assert!(matches!(
            admit_agent_binary(&bin, None),
            Err(AdmissionError::MissingMetadata(_))
        ));
    }

    #[test]
    fn rejects_malformed_metadata() {
        let good = meta("kanade-agent", "0.81.0", "linux-x86_64");
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        let mut truncated = good.clone();
        truncated.truncate(good.len() - 5);
        let mut trailing = good.clone();
        trailing.extend_from_slice(b"junk");
        let mut schema = good.clone();
        schema[8] = 9;
        let mut json = b"KNAGMETA".to_vec();
        json.extend_from_slice(&[1, 0, 0, 0, 3, 0, 0, 0]);
        json.extend_from_slice(b"{{}");
        let mut unknown = meta("kanade-agent", "0.81.0", "linux-x86_64");
        unknown.truncate(HEADER_LEN);
        let extra = br#"{"product":"kanade-agent","version":"1","platform":"linux-x86_64","x":1}"#;
        unknown[12..16].copy_from_slice(&(extra.len() as u32).to_le_bytes());
        unknown.extend_from_slice(extra);
        for (name, m) in [
            ("magic", bad_magic),
            ("truncated", truncated),
            ("trailing", trailing),
            ("json", json),
            ("unknown field", unknown),
            ("tiny", b"KNAG".to_vec()),
        ] {
            let e = admit_agent_binary(&elf(&m), None).unwrap_err();
            assert!(matches!(e, AdmissionError::Malformed(_)), "{name}: {e}");
        }
        assert!(matches!(
            admit_agent_binary(&elf(&schema), None),
            Err(AdmissionError::UnsupportedSchema(9))
        ));
    }

    #[test]
    fn rejects_duplicate_sections_and_bad_versions() {
        let m = meta("kanade-agent", "0.81.0", "linux-x86_64");
        let dup = elf_x86_64(&[("", K, &m), ("", K, &m)]);
        assert!(matches!(
            admit_agent_binary(&dup, None),
            Err(AdmissionError::DuplicateSection)
        ));
        let e = admit_agent_binary(&elf(&meta("kanade-agent", "0.8 1\n", "linux-x86_64")), None)
            .unwrap_err();
        assert!(matches!(e, AdmissionError::InvalidVersion(_)));
    }

    #[test]
    fn rejects_non_executables_and_unsupported_cpus() {
        for bytes in [&b"#!/bin/sh\necho hi\n"[..], &[0u8; 64][..], &b"MZ"[..]] {
            assert!(matches!(
                admit_agent_binary(bytes, None),
                Err(AdmissionError::UnsupportedBinary(_))
            ));
        }
        let riscv = image(
            object::BinaryFormat::Elf,
            object::Architecture::Riscv64,
            &[("", K, &meta("kanade-agent", "1.0.0", "linux-x86_64"))],
        );
        assert!(matches!(
            admit_agent_binary(&riscv, None),
            Err(AdmissionError::UnsupportedBinary(_))
        ));
    }
}
