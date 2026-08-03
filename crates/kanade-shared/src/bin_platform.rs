//! Which platform an agent binary targets, read from its own bytes — the
//! single source of truth for the `agent_releases` Object Store key scheme.
//!
//! Key scheme (backward compatible):
//!
//!   * Windows releases stay at the **bare `<version>`** key — agents in
//!     the field fetch exactly that today, so any change would be a
//!     migration. (Windows aarch64 also maps to the bare key: the fleet is
//!     x86_64 in practice, and distinguishing it is a future problem.)
//!   * Linux releases live at **`<version>-linux-<arch>`** (`x86_64` /
//!     `aarch64`). Semver prerelease dashes are fine because consumers
//!     always match the *suffix* (`-linux-x86_64` / `-linux-aarch64`),
//!     never a dash mid-version.
//!
//! Detection is by magic bytes, not filename: `MZ` → PE (arch from the
//! COFF header's Machine field), `\x7fELF` → ELF (arch from `e_machine`),
//! Mach-O magics → a clear "unsupported" error (macOS agents are out of
//! scope). Pure byte inspection, no parsing library — the PE path only
//! needs `e_lfanew` + the COFF Machine field, which `pelite` (used for
//! VERSIONINFO in `exe_version.rs`) doesn't surface as a bare number
//! without dragging in its full header model.

/// Object Store key suffix for Linux x86_64 releases.
pub const LINUX_SUFFIX_X86_64: &str = "-linux-x86_64";
/// Object Store key suffix for Linux aarch64 releases.
pub const LINUX_SUFFIX_AARCH64: &str = "-linux-aarch64";
/// Both Linux suffixes, for "any Linux key" scans (rollout checks).
pub const LINUX_SUFFIXES: [&str; 2] = [LINUX_SUFFIX_X86_64, LINUX_SUFFIX_AARCH64];

/// The platform an uploaded agent binary runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPlatform {
    WindowsX86_64,
    WindowsAarch64,
    LinuxX86_64,
    LinuxAarch64,
}

impl AgentPlatform {
    /// Identify the platform from the binary's leading bytes. Errors
    /// (rather than guesses) on Mach-O, on a recognized container with an
    /// unsupported architecture, and on anything unrecognized — a publish
    /// that can't name its platform must not silently land on the Windows
    /// key.
    pub fn detect(bytes: &[u8]) -> Result<AgentPlatform, String> {
        if bytes.starts_with(b"MZ") {
            return detect_pe(bytes);
        }
        if bytes.starts_with(b"\x7fELF") {
            return detect_elf(bytes);
        }
        if is_macho(bytes) {
            return Err(
                "macOS (Mach-O) agents are not supported — kanade-agent ships for Windows and \
                 Linux only"
                    .to_string(),
            );
        }
        Err(
            "unrecognized binary format (not a Windows PE or Linux ELF) — is this a kanade-agent \
             build?"
                .to_string(),
        )
    }

    /// The `agent_releases` Object Store key for `version` on this
    /// platform. Windows → the bare version (full backward compatibility);
    /// Linux → the arch-suffixed key.
    pub fn release_key(&self, version: &str) -> String {
        match self.suffix() {
            Some(suffix) => format!("{version}{suffix}"),
            None => version.to_string(),
        }
    }

    /// The key suffix for this platform, `None` for Windows (bare key).
    pub fn suffix(&self) -> Option<&'static str> {
        match self {
            AgentPlatform::WindowsX86_64 | AgentPlatform::WindowsAarch64 => None,
            AgentPlatform::LinuxX86_64 => Some(LINUX_SUFFIX_X86_64),
            AgentPlatform::LinuxAarch64 => Some(LINUX_SUFFIX_AARCH64),
        }
    }

    /// Human label for audit records / logs (`"windows-x86_64"`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentPlatform::WindowsX86_64 => "windows-x86_64",
            AgentPlatform::WindowsAarch64 => "windows-aarch64",
            AgentPlatform::LinuxX86_64 => "linux-x86_64",
            AgentPlatform::LinuxAarch64 => "linux-aarch64",
        }
    }
}

/// The platform label for an existing store key, derived from its suffix:
/// `"linux-x86_64"` / `"linux-aarch64"` for suffixed keys, `"windows"` for
/// anything else (bare keys are Windows by definition of the key scheme).
/// Used by the releases listing, which only has keys to look at.
pub fn platform_of_key(key: &str) -> &'static str {
    if key.ends_with(LINUX_SUFFIX_X86_64) {
        "linux-x86_64"
    } else if key.ends_with(LINUX_SUFFIX_AARCH64) {
        "linux-aarch64"
    } else {
        "windows"
    }
}

/// The rollout-visible version for a store key: the key with any linux
/// platform suffix stripped. A scope's `target_version` always names the
/// BASE version (`0.46.0`, never `0.46.0-linux-x86_64`) — the agent's own
/// platform decides which suffixed binary it fetches — so guards that
/// compare a key against `target_version` must go through this.
pub fn base_version_of_key(key: &str) -> &str {
    for suffix in LINUX_SUFFIXES {
        if let Some(base) = key.strip_suffix(suffix) {
            return base;
        }
    }
    key
}

/// Every store key a rollout's existence check should accept for
/// `version`: the bare (Windows) key plus each linux platform key. A
/// version is rollable-out when ANY of these exists — a Linux-only
/// publish never writes the bare key.
pub fn candidate_keys(version: &str) -> Vec<String> {
    let mut keys = Vec::with_capacity(1 + LINUX_SUFFIXES.len());
    keys.push(version.to_string());
    for suffix in LINUX_SUFFIXES {
        keys.push(format!("{version}{suffix}"));
    }
    keys
}

/// The charset every `agent_releases` key must fit: the key reaches a
/// quoted `Content-Disposition` filename, generated PowerShell / batch /
/// shell install scripts, and NATS subjects — restrict the charset
/// (semver-ish) rather than escaping four different formats. Shared by the
/// backend publish endpoint, the CLI publish, and the installer endpoint
/// (which used to carry its own copy).
pub fn check_release_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
    {
        return Err(format!(
            "release key must be non-empty and contain only [A-Za-z0-9._+-], got {key:?}"
        ));
    }
    Ok(())
}

/// PE: `MZ` DOS stub, `e_lfanew` (u32 LE at 0x3C) → `"PE\0\0"` signature,
/// COFF Machine field (u16 LE right after). 0x8664 = x86_64, 0xAA64 =
/// aarch64 (ARM64EC uses the same field for our purposes — a native ARM64
/// agent build reports 0xAA64).
fn detect_pe(bytes: &[u8]) -> Result<AgentPlatform, String> {
    let truncated = || "truncated PE header".to_string();
    let pe_off = u32::from_le_bytes(
        bytes
            .get(0x3C..0x40)
            .ok_or_else(truncated)?
            .try_into()
            .map_err(|_| truncated())?,
    ) as usize;
    let coff = bytes.get(pe_off..pe_off + 6).ok_or_else(truncated)?;
    if coff[..4] != *b"PE\0\0" {
        return Err("MZ binary without a PE signature".to_string());
    }
    match u16::from_le_bytes([coff[4], coff[5]]) {
        0x8664 => Ok(AgentPlatform::WindowsX86_64),
        0xAA64 => Ok(AgentPlatform::WindowsAarch64),
        other => Err(format!(
            "unsupported PE machine type 0x{other:04X} (kanade-agent ships x86_64 and aarch64 only)"
        )),
    }
}

/// ELF: magic, then `e_machine` (u16 at offset 18) read with the
/// endianness `EI_DATA` (offset 5) declares. 62 = EM_X86_64, 183 =
/// EM_AARCH64.
fn detect_elf(bytes: &[u8]) -> Result<AgentPlatform, String> {
    let truncated = || "truncated ELF header".to_string();
    let ei_data = bytes.get(5).ok_or_else(truncated)?;
    let em = bytes.get(18..20).ok_or_else(truncated)?;
    let machine = match ei_data {
        1 => u16::from_le_bytes([em[0], em[1]]),
        2 => u16::from_be_bytes([em[0], em[1]]),
        other => return Err(format!("unknown ELF endianness {other}")),
    };
    match machine {
        62 => Ok(AgentPlatform::LinuxX86_64),
        183 => Ok(AgentPlatform::LinuxAarch64),
        other => Err(format!(
            "unsupported ELF machine {other} (kanade-agent ships x86_64 (62) and aarch64 (183) \
             only)"
        )),
    }
}

/// Mach-O magics, both endiannesses, 32/64-bit, plus the fat (universal)
/// wrappers — anything Apple is out of scope, so they all funnel to the
/// same "unsupported" error rather than a misdetection.
fn is_macho(bytes: &[u8]) -> bool {
    const MAGICS: [[u8; 4]; 8] = [
        [0xFE, 0xED, 0xFA, 0xCE], // MH_MAGIC (32-bit, BE)
        [0xCE, 0xFA, 0xED, 0xFE], // MH_CIGAM (32-bit, LE)
        [0xFE, 0xED, 0xFA, 0xCF], // MH_MAGIC_64 (BE)
        [0xCF, 0xFA, 0xED, 0xFE], // MH_CIGAM_64 (LE)
        [0xCA, 0xFE, 0xBA, 0xBE], // FAT_MAGIC (universal, BE)
        [0xBE, 0xBA, 0xFE, 0xCA], // FAT_CIGAM
        [0xCA, 0xFE, 0xBA, 0xBF], // FAT_MAGIC_64
        [0xBF, 0xBA, 0xFE, 0xCA], // FAT_CIGAM_64
    ];
    MAGICS.iter().any(|m| bytes.starts_with(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal MZ+PE: DOS stub with e_lfanew at 0x80, signature + Machine.
    fn fake_pe(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x80 + 6];
        b[0] = b'M';
        b[1] = b'Z';
        b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        b
    }

    /// Minimal ELF ident + e_machine.
    fn fake_elf(machine: u16, little_endian: bool) -> Vec<u8> {
        let mut b = vec![0u8; 20];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // ELFCLASS64
        b[5] = if little_endian { 1 } else { 2 };
        if little_endian {
            b[18..20].copy_from_slice(&machine.to_le_bytes());
        } else {
            b[18..20].copy_from_slice(&machine.to_be_bytes());
        }
        b
    }

    #[test]
    fn detects_pe_architectures() {
        assert_eq!(
            AgentPlatform::detect(&fake_pe(0x8664)).unwrap(),
            AgentPlatform::WindowsX86_64
        );
        assert_eq!(
            AgentPlatform::detect(&fake_pe(0xAA64)).unwrap(),
            AgentPlatform::WindowsAarch64
        );
        // An MZ with a machine we don't ship is an error, not a guess.
        assert!(AgentPlatform::detect(&fake_pe(0x14C)).is_err()); // i386
        // MZ but no PE signature (a DOS binary) is an error too.
        let mut b = fake_pe(0x8664);
        b[0x80] = b'X';
        assert!(AgentPlatform::detect(&b).is_err());
        // Truncated just past MZ.
        assert!(AgentPlatform::detect(b"MZ").is_err());
    }

    #[test]
    fn detects_elf_architectures_and_endianness() {
        assert_eq!(
            AgentPlatform::detect(&fake_elf(62, true)).unwrap(),
            AgentPlatform::LinuxX86_64
        );
        assert_eq!(
            AgentPlatform::detect(&fake_elf(183, true)).unwrap(),
            AgentPlatform::LinuxAarch64
        );
        // e_machine honors EI_DATA — a big-endian-encoded aarch64 still
        // reads as aarch64.
        assert_eq!(
            AgentPlatform::detect(&fake_elf(183, false)).unwrap(),
            AgentPlatform::LinuxAarch64
        );
        assert!(AgentPlatform::detect(&fake_elf(40, true)).is_err()); // ARM 32
        assert!(AgentPlatform::detect(b"\x7fELF").is_err()); // truncated
    }

    #[test]
    fn macho_is_a_clear_unsupported_error() {
        for magic in [
            [0xFE, 0xED, 0xFA, 0xCE],
            [0xCF, 0xFA, 0xED, 0xFE],
            [0xCA, 0xFE, 0xBA, 0xBE],
        ] {
            let err = AgentPlatform::detect(&magic).unwrap_err();
            assert!(err.contains("macOS"), "{err}");
        }
    }

    #[test]
    fn unknown_bytes_are_an_error_not_a_windows_guess() {
        assert!(AgentPlatform::detect(b"#!/bin/sh\necho hi").is_err());
        assert!(AgentPlatform::detect(b"").is_err());
    }

    #[test]
    fn release_keys_follow_the_scheme() {
        // Windows stays bare — the whole backward-compatibility point.
        assert_eq!(AgentPlatform::WindowsX86_64.release_key("0.45.4"), "0.45.4");
        assert_eq!(
            AgentPlatform::WindowsAarch64.release_key("0.45.4"),
            "0.45.4"
        );
        assert_eq!(
            AgentPlatform::LinuxX86_64.release_key("0.45.4"),
            "0.45.4-linux-x86_64"
        );
        assert_eq!(
            AgentPlatform::LinuxAarch64.release_key("0.45.4"),
            "0.45.4-linux-aarch64"
        );
        // Semver prerelease dashes are untouched — only the suffix matters.
        assert_eq!(
            AgentPlatform::LinuxX86_64.release_key("0.46.0-rc.1"),
            "0.46.0-rc.1-linux-x86_64"
        );
        assert!(check_release_key(&AgentPlatform::LinuxAarch64.release_key("0.46.0-rc.1")).is_ok());
    }

    #[test]
    fn platform_of_key_reads_the_suffix_only() {
        assert_eq!(platform_of_key("0.45.4"), "windows");
        assert_eq!(platform_of_key("0.45.4-linux-x86_64"), "linux-x86_64");
        assert_eq!(platform_of_key("0.45.4-linux-aarch64"), "linux-aarch64");
        // A version whose PRERELEASE mentions linux still parses by suffix:
        // `-linux-x86_64` wins, but a bare `1.0.0-linux` is a Windows key
        // (no arch suffix — odd, but the suffix rule is the contract).
        assert_eq!(platform_of_key("1.0.0-linux"), "windows");
        assert_eq!(platform_of_key("1.0.0-rc-linux-x86_64"), "linux-x86_64");
    }

    #[test]
    fn base_version_strips_only_the_platform_suffix() {
        assert_eq!(base_version_of_key("0.45.4"), "0.45.4");
        assert_eq!(base_version_of_key("0.45.4-linux-x86_64"), "0.45.4");
        assert_eq!(base_version_of_key("0.45.4-linux-aarch64"), "0.45.4");
        // Prerelease dashes are not platform suffixes.
        assert_eq!(base_version_of_key("0.46.0-rc.1"), "0.46.0-rc.1");
        assert_eq!(
            base_version_of_key("0.46.0-rc.1-linux-x86_64"),
            "0.46.0-rc.1"
        );
    }

    #[test]
    fn candidate_keys_cover_bare_then_linux() {
        assert_eq!(
            candidate_keys("0.46.0"),
            vec![
                "0.46.0".to_string(),
                "0.46.0-linux-x86_64".to_string(),
                "0.46.0-linux-aarch64".to_string(),
            ]
        );
    }

    #[test]
    fn release_key_charset_is_restricted() {
        for bad in ["", "0.43.99\n evil", "0.43.99\"x", "0.43.99'x", "a b"] {
            assert!(check_release_key(bad).is_err(), "{bad:?}");
        }
        for good in [
            "0.43.99",
            "0.43.99-rc.1+build.5",
            "1.0.0_alpha",
            "0.45.4-linux-x86_64",
        ] {
            check_release_key(good).unwrap();
        }
    }
}
