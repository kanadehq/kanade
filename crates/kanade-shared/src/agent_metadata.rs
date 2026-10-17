use std::borrow::Cow;
use std::fmt;

use object::read::Object;
use serde::{Deserialize, Serialize};

/// Section name used for Windows PE and Linux ELF binaries.
pub const AGENT_METADATA_SECTION_PE_ELF: &str = ".kanade";

/// Section name used for macOS Mach-O binaries.
pub const AGENT_METADATA_SECTION_MACHO: &str = "__DATA,__kanade";

/// Binary magic header (8 bytes): "KNAGMETA"
pub const AGENT_METADATA_MAGIC: &[u8; 8] = b"KNAGMETA";

/// Current schema version of the binary metadata wrapper.
pub const AGENT_METADATA_SCHEMA_VERSION: u16 = 1;

/// Fixed 16-byte envelope header:
/// 8 bytes: Magic ("KNAGMETA")
/// 2 bytes: Schema version (u16 LE)
/// 2 bytes: Flags (u16 LE, 0 for v1)
/// 4 bytes: Payload length (u32 LE)
pub const AGENT_METADATA_HEADER_LEN: usize = 16;

/// Canonical product identifier for kanade-agent.
pub const AGENT_PRODUCT_NAME: &str = "kanade-agent";

/// Structured metadata payload embedded into the agent binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBinaryMetadata {
    pub product: String,
    pub version: String,
    pub platform: AgentPlatform,
}

/// Supported binary platform target triple identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentPlatform {
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "linux-aarch64")]
    LinuxAarch64,
    #[serde(rename = "macos-aarch64")]
    MacosAarch64,
}

impl AgentPlatform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "windows-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
            Self::MacosAarch64 => "macos-aarch64",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "windows-x86_64" => Some(Self::WindowsX86_64),
            "linux-x86_64" => Some(Self::LinuxX86_64),
            "linux-aarch64" => Some(Self::LinuxAarch64),
            "macos-aarch64" => Some(Self::MacosAarch64),
            _ => None,
        }
    }

    pub fn file_suffix(&self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "windows-x86_64.exe",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
            Self::MacosAarch64 => "macos-aarch64",
        }
    }

    /// Detect container format and architecture from raw binary bytes.
    pub fn detect_container(bytes: &[u8]) -> Result<Option<Self>, MetadataError> {
        if bytes.len() < 16 {
            return Ok(None);
        }

        // PE check
        if bytes.starts_with(b"MZ") && bytes.len() >= 0x40 {
            if let Ok(file) = object::File::parse(bytes) {
                if file.format() == object::BinaryFormat::Pe {
                    return match file.architecture() {
                        object::Architecture::X86_64 => Ok(Some(Self::WindowsX86_64)),
                        _ => Err(MetadataError::UnsupportedPlatform(format!(
                            "unsupported PE architecture: {:?}",
                            file.architecture()
                        ))),
                    };
                }
            }
        }

        // ELF check
        if bytes.starts_with(b"\x7fELF") {
            if let Ok(file) = object::File::parse(bytes) {
                if file.format() == object::BinaryFormat::Elf {
                    return match file.architecture() {
                        object::Architecture::X86_64 => Ok(Some(Self::LinuxX86_64)),
                        object::Architecture::Aarch64 => Ok(Some(Self::LinuxAarch64)),
                        _ => Err(MetadataError::UnsupportedPlatform(format!(
                            "unsupported ELF architecture: {:?}",
                            file.architecture()
                        ))),
                    };
                }
            }
        }

        // Mach-O check
        if bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xcf]) || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
            if let Ok(file) = object::File::parse(bytes) {
                if file.format() == object::BinaryFormat::MachO {
                    return match file.architecture() {
                        object::Architecture::Aarch64 => Ok(Some(Self::MacosAarch64)),
                        _ => Err(MetadataError::UnsupportedPlatform(format!(
                            "unsupported Mach-O architecture: {:?}",
                            file.architecture()
                        ))),
                    };
                }
            }
        }

        Ok(None)
    }
}

impl fmt::Display for AgentPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Errors that can occur during agent metadata validation.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("binary is too small or not a recognized executable container (PE/ELF/Mach-O)")]
    UnrecognizedContainer,

    #[error("container architecture or platform is unsupported: {0}")]
    UnsupportedPlatform(String),

    #[error("legacy binary or missing agent release metadata: required section (.kanade / __DATA,__kanade) was not found")]
    MissingMetadata,

    #[error("duplicate agent metadata sections found in binary")]
    DuplicateSection,

    #[error("invalid metadata magic: expected KNAGMETA")]
    InvalidMagic,

    #[error("unsupported metadata schema version: {0}")]
    UnsupportedSchema(u16),

    #[error("metadata payload length mismatch or corrupted envelope")]
    PayloadCorrupted,

    #[error("trailing garbage bytes found after metadata payload")]
    TrailingData,

    #[error("failed to deserialize metadata JSON payload: {0}")]
    JsonDeserialization(serde_json::Error),

    #[error("product mismatch: expected '{AGENT_PRODUCT_NAME}', found '{0}'")]
    ProductMismatch(String),

    #[error("platform mismatch: binary container is '{container}', but embedded metadata specifies '{embedded}'")]
    PlatformMismatch {
        container: AgentPlatform,
        embedded: AgentPlatform,
    },

    #[error("version mismatch: expected '{expected}', but embedded binary version is '{embedded}'")]
    VersionMismatch {
        expected: String,
        embedded: String,
    },

    #[error("failed to parse executable object file: {0}")]
    ObjectParse(String),
}

/// Encodes an `AgentBinaryMetadata` struct into binary bytes ready to embed into
/// `.kanade` / `__DATA,__kanade` section.
pub fn encode_metadata_bytes(meta: &AgentBinaryMetadata) -> Vec<u8> {
    let json_bytes = serde_json::to_vec(meta).expect("serializing agent metadata to JSON");
    let mut out = Vec::with_capacity(AGENT_METADATA_HEADER_LEN + json_bytes.len());
    out.extend_from_slice(AGENT_METADATA_MAGIC);
    out.extend_from_slice(&AGENT_METADATA_SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&json_bytes);
    out
}

/// Parses the raw metadata bytes from the section into `AgentBinaryMetadata`.
pub fn parse_metadata_bytes(raw: &[u8]) -> Result<AgentBinaryMetadata, MetadataError> {
    if raw.len() < AGENT_METADATA_HEADER_LEN {
        return Err(MetadataError::PayloadCorrupted);
    }

    if &raw[0..8] != AGENT_METADATA_MAGIC {
        return Err(MetadataError::InvalidMagic);
    }

    let schema_ver = u16::from_le_bytes([raw[8], raw[9]]);
    if schema_ver != AGENT_METADATA_SCHEMA_VERSION {
        return Err(MetadataError::UnsupportedSchema(schema_ver));
    }

    let payload_len = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
    if raw.len() < AGENT_METADATA_HEADER_LEN + payload_len {
        return Err(MetadataError::PayloadCorrupted);
    }

    let payload_bytes = &raw[AGENT_METADATA_HEADER_LEN..AGENT_METADATA_HEADER_LEN + payload_len];

    // Check for trailing non-zero bytes
    let rest = &raw[AGENT_METADATA_HEADER_LEN + payload_len..];
    if rest.iter().any(|&b| b != 0) {
        return Err(MetadataError::TrailingData);
    }

    let meta: AgentBinaryMetadata =
        serde_json::from_slice(payload_bytes).map_err(MetadataError::JsonDeserialization)?;
    Ok(meta)
}

/// Extracts raw bytes of the `.kanade` or `__DATA,__kanade` section from a binary.
pub fn extract_raw_metadata_section<'a>(bytes: &'a [u8]) -> Result<Option<Cow<'a, [u8]>>, MetadataError> {
    let file = object::File::parse(bytes).map_err(|e| MetadataError::ObjectParse(e.to_string()))?;

    let mut found: Option<Cow<'a, [u8]>> = None;

    for section in file.sections() {
        use object::ObjectSection;
        let name = section.name().unwrap_or_default();
        if name == AGENT_METADATA_SECTION_PE_ELF || name == AGENT_METADATA_SECTION_MACHO || name == "__kanade" {
            if found.is_some() {
                return Err(MetadataError::DuplicateSection);
            }
            let data = section.data().map_err(|e| MetadataError::ObjectParse(e.to_string()))?;
            found = Some(Cow::Borrowed(data));
        }
    }

    Ok(found)
}

/// Extracts and parses the `AgentBinaryMetadata` from raw binary bytes.
pub fn extract_agent_metadata(bytes: &[u8]) -> Result<(AgentBinaryMetadata, AgentPlatform), MetadataError> {
    let container = AgentPlatform::detect_container(bytes)?
        .ok_or(MetadataError::UnrecognizedContainer)?;

    let raw_section = extract_raw_metadata_section(bytes)?
        .ok_or(MetadataError::MissingMetadata)?;

    let metadata = parse_metadata_bytes(&raw_section)?;

    if metadata.product != AGENT_PRODUCT_NAME {
        return Err(MetadataError::ProductMismatch(metadata.product));
    }

    if metadata.platform != container {
        return Err(MetadataError::PlatformMismatch {
            container,
            embedded: metadata.platform,
        });
    }

    // Additional check for Windows PE: verify VERSIONINFO string table if present
    if container == AgentPlatform::WindowsX86_64 {
        if let Some(pe_ver) = crate::exe_version::extract_pe_version(bytes) {
            let norm_pe = normalize_version(&pe_ver);
            let norm_meta = normalize_version(&metadata.version);
            if norm_pe != norm_meta {
                return Err(MetadataError::VersionMismatch {
                    expected: pe_ver,
                    embedded: metadata.version.clone(),
                });
            }
        }
    }

    Ok((metadata, container))
}

/// Normalizes version strings for comparison by trimming whitespace and stripping a leading 'v'.
pub fn normalize_version(v: &str) -> &str {
    let trimmed = v.trim();
    trimmed.strip_prefix('v').unwrap_or(trimmed)
}

/// Result of admitting an agent binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedAgentBinary {
    pub metadata: AgentBinaryMetadata,
    pub platform: AgentPlatform,
    pub key: String,
}

/// Comprehensive admission validation for agent binaries:
/// - Validates container format and architecture.
/// - Requires valid `.kanade` / `__DATA,__kanade` embedded metadata.
/// - Validates magic, schema version, product name (`kanade-agent`), and platform consistency.
/// - If `declared_version` is provided, ensures embedded version matches (ignoring leading 'v').
/// - Rejects legacy binaries lacking metadata (does not accept unverifiable binaries).
pub fn admit_agent_binary(
    bytes: &[u8],
    declared_version: Option<&str>,
) -> Result<AdmittedAgentBinary, MetadataError> {
    let (metadata, platform) = extract_agent_metadata(bytes)?;

    if let Some(expected_ver) = declared_version {
        let norm_expected = normalize_version(expected_ver);
        let norm_embedded = normalize_version(&metadata.version);
        if norm_expected != norm_embedded {
            return Err(MetadataError::VersionMismatch {
                expected: expected_ver.to_string(),
                embedded: metadata.version.clone(),
            });
        }
    }

    let version = normalize_version(&metadata.version);
    let key = format!("{}-{}.exe", version, platform.as_str());

    Ok(AdmittedAgentBinary {
        metadata,
        platform,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::write::Object;
    use object::{Architecture, BinaryFormat, Endianness};

    fn make_test_elf(arch: Architecture, section_name: &[u8], section_data: &[u8]) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Elf, arch, Endianness::Little);
        let sec_id = obj.add_section(
            Vec::new(),
            section_name.to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        obj.append_section_data(sec_id, section_data, 1);
        obj.write().expect("writing test ELF")
    }

    fn make_test_macho(arch: Architecture, section_data: &[u8]) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::MachO, arch, Endianness::Little);
        let sec_id = obj.add_section(
            b"__DATA".to_vec(),
            b"__kanade".to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        obj.append_section_data(sec_id, section_data, 1);
        obj.write().expect("writing test Mach-O")
    }

    fn make_test_pe(section_name: &[u8], section_data: &[u8]) -> Vec<u8> {
        let mut pe = vec![0u8; 0x80];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes()); // e_lfanew -> 0x80

        // PE Header at 0x80
        pe.extend_from_slice(b"PE\0\0"); // Signature
        pe.extend_from_slice(&0x8664u16.to_le_bytes()); // Machine: IMAGE_FILE_MACHINE_AMD64
        pe.extend_from_slice(&1u16.to_le_bytes()); // NumberOfSections: 1
        pe.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
        pe.extend_from_slice(&0u32.to_le_bytes()); // PointerToSymbolTable
        pe.extend_from_slice(&0u32.to_le_bytes()); // NumberOfSymbols
        pe.extend_from_slice(&0xF0u16.to_le_bytes()); // SizeOfOptionalHeader
        pe.extend_from_slice(&0x0022u16.to_le_bytes()); // Characteristics: EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE

        // Optional Header (PE32+) - 240 bytes (0xF0)
        let opt_hdr_start = pe.len();
        pe.extend_from_slice(&0x020Bu16.to_le_bytes()); // Magic: PE32+
        pe.resize(opt_hdr_start + 0xF0, 0);

        // Section header - 40 bytes
        let mut sec_hdr = [0u8; 40];
        let copy_len = section_name.len().min(8);
        sec_hdr[..copy_len].copy_from_slice(&section_name[..copy_len]);
        sec_hdr[8..12].copy_from_slice(&(section_data.len() as u32).to_le_bytes()); // VirtualSize
        sec_hdr[12..16].copy_from_slice(&0x1000u32.to_le_bytes()); // VirtualAddress
        sec_hdr[16..20].copy_from_slice(&(section_data.len() as u32).to_le_bytes()); // SizeOfRawData
        sec_hdr[20..24].copy_from_slice(&0x200u32.to_le_bytes()); // PointerToRawData -> 0x200 (512)
        sec_hdr[36..40].copy_from_slice(&0x40000040u32.to_le_bytes()); // Characteristics: INITIALIZED_DATA | READ
        pe.extend_from_slice(&sec_hdr);

        // Pad to PointerToRawData (0x200)
        if pe.len() < 0x200 {
            pe.resize(0x200, 0);
        }
        pe.extend_from_slice(section_data);

        pe
    }

    #[test]
    fn test_valid_elf_x86_64_admitted() {
        let meta = AgentBinaryMetadata {
            product: AGENT_PRODUCT_NAME.to_string(),
            version: "0.80.0".to_string(),
            platform: AgentPlatform::LinuxX86_64,
        };
        let encoded = encode_metadata_bytes(&meta);
        let bytes = make_test_elf(Architecture::X86_64, b".kanade", &encoded);

        let admitted = admit_agent_binary(&bytes, Some("0.80.0")).expect("admission must succeed");
        assert_eq!(admitted.platform, AgentPlatform::LinuxX86_64);
        assert_eq!(admitted.metadata.version, "0.80.0");
        assert_eq!(admitted.key, "0.80.0-linux-x86_64.exe");
    }

    #[test]
    fn test_valid_elf_aarch64_admitted() {
        let meta = AgentBinaryMetadata {
            product: AGENT_PRODUCT_NAME.to_string(),
            version: "0.80.0".to_string(),
            platform: AgentPlatform::LinuxAarch64,
        };
        let encoded = encode_metadata_bytes(&meta);
        let bytes = make_test_elf(Architecture::Aarch64, b".kanade", &encoded);

        let admitted = admit_agent_binary(&bytes, Some("v0.80.0")).expect("admission with v-prefix");
        assert_eq!(admitted.platform, AgentPlatform::LinuxAarch64);
        assert_eq!(admitted.metadata.version, "0.80.0");
        assert_eq!(admitted.key, "0.80.0-linux-aarch64.exe");
    }

    #[test]
    fn test_valid_macho_arm64_admitted() {
        let meta = AgentBinaryMetadata {
            product: AGENT_PRODUCT_NAME.to_string(),
            version: "0.80.0".to_string(),
            platform: AgentPlatform::MacosAarch64,
        };
        let encoded = encode_metadata_bytes(&meta);
        let bytes = make_test_macho(Architecture::Aarch64, &encoded);

        let admitted = admit_agent_binary(&bytes, None).expect("admission without explicit version");
        assert_eq!(admitted.platform, AgentPlatform::MacosAarch64);
        assert_eq!(admitted.metadata.version, "0.80.0");
        assert_eq!(admitted.key, "0.80.0-macos-aarch64.exe");
    }

    #[test]
    fn test_valid_pe_x86_64_admitted() {
        let meta = AgentBinaryMetadata {
            product: AGENT_PRODUCT_NAME.to_string(),
            version: "0.80.0".to_string(),
            platform: AgentPlatform::WindowsX86_64,
        };
        let encoded = encode_metadata_bytes(&meta);
        let bytes = make_test_pe(b".kanade", &encoded);

        let admitted = admit_agent_binary(&bytes, Some("0.80.0")).expect("admission for PE");
        assert_eq!(admitted.platform, AgentPlatform::WindowsX86_64);
        assert_eq!(admitted.metadata.version, "0.80.0");
        assert_eq!(admitted.key, "0.80.0-windows-x86_64.exe");
    }

    #[test]
    fn test_legacy_binary_lacking_metadata_is_rejected() {
        let bytes = make_test_elf(Architecture::X86_64, b".rodata", b"hello world");
        let err = admit_agent_binary(&bytes, Some("0.80.0")).unwrap_err();
        match err {
            MetadataError::MissingMetadata => {}
            other => panic!("expected MissingMetadata, got {other:?}"),
        }
    }

    #[test]
    fn test_wrong_product_is_rejected() {
        let meta = AgentBinaryMetadata {
            product: "kanade-cli".to_string(), // incident simulation
            version: "0.80.0".to_string(),
            platform: AgentPlatform::MacosAarch64,
        };
        let encoded = encode_metadata_bytes(&meta);
        let bytes = make_test_macho(Architecture::Aarch64, &encoded);

        let err = admit_agent_binary(&bytes, Some("0.80.0")).unwrap_err();
        match err {
            MetadataError::ProductMismatch(p) => assert_eq!(p, "kanade-cli"),
            other => panic!("expected ProductMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_wrong_version_is_rejected() {
        let meta = AgentBinaryMetadata {
            product: AGENT_PRODUCT_NAME.to_string(),
            version: "0.79.0".to_string(),
            platform: AgentPlatform::LinuxX86_64,
        };
        let encoded = encode_metadata_bytes(&meta);
        let bytes = make_test_elf(Architecture::X86_64, b".kanade", &encoded);

        let err = admit_agent_binary(&bytes, Some("0.80.0")).unwrap_err();
        match err {
            MetadataError::VersionMismatch { expected, embedded } => {
                assert_eq!(expected, "0.80.0");
                assert_eq!(embedded, "0.79.0");
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_platform_mismatch_is_rejected() {
        let meta = AgentBinaryMetadata {
            product: AGENT_PRODUCT_NAME.to_string(),
            version: "0.80.0".to_string(),
            platform: AgentPlatform::WindowsX86_64, // embedded PE platform in ELF container
        };
        let encoded = encode_metadata_bytes(&meta);
        let bytes = make_test_elf(Architecture::X86_64, b".kanade", &encoded);

        let err = admit_agent_binary(&bytes, Some("0.80.0")).unwrap_err();
        match err {
            MetadataError::PlatformMismatch { container, embedded } => {
                assert_eq!(container, AgentPlatform::LinuxX86_64);
                assert_eq!(embedded, AgentPlatform::WindowsX86_64);
            }
            other => panic!("expected PlatformMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_corrupted_payload_is_rejected() {
        let mut encoded = b"KNAGMETA".to_vec();
        encoded.extend_from_slice(&1u16.to_le_bytes()); // ver 1
        encoded.extend_from_slice(&0u16.to_le_bytes()); // flags
        encoded.extend_from_slice(&100u32.to_le_bytes()); // claims 100 bytes
        encoded.extend_from_slice(b"{ truncated"); // only 11 bytes

        let bytes = make_test_elf(Architecture::X86_64, b".kanade", &encoded);
        let err = admit_agent_binary(&bytes, Some("0.80.0")).unwrap_err();
        match err {
            MetadataError::PayloadCorrupted => {}
            other => panic!("expected PayloadCorrupted, got {other:?}"),
        }
    }

    #[test]
    fn test_trailing_garbage_is_rejected() {
        let meta = AgentBinaryMetadata {
            product: AGENT_PRODUCT_NAME.to_string(),
            version: "0.80.0".to_string(),
            platform: AgentPlatform::LinuxX86_64,
        };
        let mut encoded = encode_metadata_bytes(&meta);
        encoded.extend_from_slice(b"GARBAGE_BYTES");

        let bytes = make_test_elf(Architecture::X86_64, b".kanade", &encoded);
        let err = admit_agent_binary(&bytes, Some("0.80.0")).unwrap_err();
        match err {
            MetadataError::TrailingData => {}
            other => panic!("expected TrailingData, got {other:?}"),
        }
    }
}
