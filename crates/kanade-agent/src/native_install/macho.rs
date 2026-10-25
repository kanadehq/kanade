//! Structural check that extracted bytes are an arm64 macOS executable.
//!
//! This is deliberately a small, bounds-checked reader of just the header and
//! load-command table (no symbol or segment parsing). It refuses universal
//! ("fat") binaries, 32-bit images, non-arm64 CPUs, non-executable file types
//! and anything whose load commands are malformed, and it requires an
//! `LC_BUILD_VERSION` for the macOS platform whose minimum OS is no newer than
//! the oldest OS this handler supports.

const MH_MAGIC_64: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MH_EXECUTE: u32 = 2;
const LC_BUILD_VERSION: u32 = 0x32;
const PLATFORM_MACOS: u32 = 1;
const HEADER_LEN: usize = 32;
const MAX_LOAD_COMMANDS: u32 = 4096;
const MAX_LOAD_COMMANDS_BYTES: u32 = 1 << 20;

/// Oldest supported host: macOS 14.0, encoded `xxxx.yy.zz`.
pub const SUPPORTED_MIN_OS: u32 = 14 << 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MachOError {
    #[error("not a thin 64-bit little-endian Mach-O image")]
    NotMachO,
    #[error("executable is not built for arm64")]
    WrongArchitecture,
    #[error("Mach-O file type is not an executable")]
    NotExecutable,
    #[error("Mach-O load commands are malformed")]
    Malformed,
    #[error("executable has no macOS build version")]
    NoBuildVersion,
    #[error("executable requires macOS newer than 14.0")]
    MinOsTooNew,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// The minimum macOS version the image declares, as `xxxx.yy.zz`.
pub fn check_arm64_executable(bytes: &[u8]) -> Result<u32, MachOError> {
    let magic = u32_at(bytes, 0).ok_or(MachOError::NotMachO)?;
    if magic != MH_MAGIC_64 {
        // Covers fat (0xcafebabe / 0xbebafeca), 32-bit and big-endian images.
        return Err(MachOError::NotMachO);
    }
    let h = |at| u32_at(bytes, at).ok_or(MachOError::Malformed);
    if h(4)? != CPU_TYPE_ARM64 {
        return Err(MachOError::WrongArchitecture);
    }
    if h(12)? != MH_EXECUTE {
        return Err(MachOError::NotExecutable);
    }
    let ncmds = h(16)?;
    let sizeofcmds = h(20)?;
    if ncmds == 0 || ncmds > MAX_LOAD_COMMANDS || sizeofcmds > MAX_LOAD_COMMANDS_BYTES {
        return Err(MachOError::Malformed);
    }
    let end = HEADER_LEN
        .checked_add(sizeofcmds as usize)
        .filter(|e| *e <= bytes.len())
        .ok_or(MachOError::Malformed)?;
    let mut at = HEADER_LEN;
    let mut minos = None;
    for _ in 0..ncmds {
        let cmd = u32_at(bytes, at).ok_or(MachOError::Malformed)?;
        let size = u32_at(bytes, at + 4).ok_or(MachOError::Malformed)? as usize;
        if size < 8 || size % 8 != 0 || at + size > end {
            return Err(MachOError::Malformed);
        }
        if cmd == LC_BUILD_VERSION {
            if size < 24 {
                return Err(MachOError::Malformed);
            }
            let platform = u32_at(bytes, at + 8).ok_or(MachOError::Malformed)?;
            let v = u32_at(bytes, at + 12).ok_or(MachOError::Malformed)?;
            if platform == PLATFORM_MACOS && minos.replace(v).is_some() {
                return Err(MachOError::Malformed);
            }
        }
        at += size;
    }
    let minos = minos.ok_or(MachOError::NoBuildVersion)?;
    if minos > SUPPORTED_MIN_OS {
        return Err(MachOError::MinOsTooNew);
    }
    Ok(minos)
}

#[cfg(test)]
pub(crate) mod test_support {
    /// A minimal thin arm64 executable image with one `LC_BUILD_VERSION`,
    /// padded with `tail` so distinct payloads get distinct digests.
    pub fn image(cputype: u32, filetype: u32, minos_major: u32, tail: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [super::MH_MAGIC_64, cputype, 0, filetype, 1, 24, 0, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            super::LC_BUILD_VERSION,
            24,
            super::PLATFORM_MACOS,
            minos_major << 16,
            0,
            0,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(tail);
        b
    }

    pub fn arm64(tail: &[u8]) -> Vec<u8> {
        image(super::CPU_TYPE_ARM64, super::MH_EXECUTE, 14, tail)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn accepts_a_thin_arm64_executable() {
        assert_eq!(check_arm64_executable(&arm64(b"x")), Ok(14 << 16));
        assert_eq!(
            check_arm64_executable(&image(CPU_TYPE_ARM64, MH_EXECUTE, 11, b"")),
            Ok(11 << 16)
        );
    }

    #[test]
    fn wrong_architecture_type_or_min_os_is_refused() {
        // x86_64
        assert_eq!(
            check_arm64_executable(&image(0x0100_0007, MH_EXECUTE, 14, b"")),
            Err(MachOError::WrongArchitecture)
        );
        // dylib
        assert_eq!(
            check_arm64_executable(&image(CPU_TYPE_ARM64, 6, 14, b"")),
            Err(MachOError::NotExecutable)
        );
        assert_eq!(
            check_arm64_executable(&image(CPU_TYPE_ARM64, MH_EXECUTE, 15, b"")),
            Err(MachOError::MinOsTooNew)
        );
    }

    #[test]
    fn fat_elf_and_truncated_images_are_refused() {
        assert_eq!(
            check_arm64_executable(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2]),
            Err(MachOError::NotMachO)
        );
        assert_eq!(
            check_arm64_executable(b"\x7fELF\x02\x01\x01\x00rest"),
            Err(MachOError::NotMachO)
        );
        assert_eq!(check_arm64_executable(b""), Err(MachOError::NotMachO));
        let full = arm64(b"");
        for n in 4..full.len() {
            assert!(check_arm64_executable(&full[..n]).is_err(), "len {n}");
        }
    }

    #[test]
    fn malformed_load_commands_are_refused() {
        let mut bad = arm64(b"");
        bad[36..40].copy_from_slice(&1000u32.to_le_bytes()); // cmdsize past the table
        assert_eq!(check_arm64_executable(&bad), Err(MachOError::Malformed));
        let mut none = arm64(b"");
        none[32..36].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64, not a build version
        assert_eq!(
            check_arm64_executable(&none),
            Err(MachOError::NoBuildVersion)
        );
    }
}
