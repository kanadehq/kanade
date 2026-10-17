//! Release metadata embedded into the agent binary.
//!
//! This section is compiled into a dedicated section (`.kanade` for Windows PE
//! and Linux ELF, `__DATA,__kanade` for macOS Mach-O). It contains the canonical
//! product identifier ("kanade-agent"), release version, and target platform.
//!
//! The static array is marked `#[used]` to prevent removal by dead-code elimination,
//! strip, optimization, and macOS codesigning.

const METADATA_LEN: usize = include_bytes!(concat!(env!("OUT_DIR"), "/agent_metadata.bin")).len();

#[used]
#[cfg_attr(
    any(target_os = "windows", target_os = "linux"),
    unsafe(link_section = ".kanade")
)]
#[cfg_attr(
    target_os = "macos",
    unsafe(link_section = "__DATA,__kanade")
)]
pub static AGENT_RELEASE_METADATA: [u8; METADATA_LEN] =
    *include_bytes!(concat!(env!("OUT_DIR"), "/agent_metadata.bin"));

/// Ensures the release metadata symbol is not dropped by whole-program optimization / LTO.
#[inline(never)]
pub fn ensure_embedded() -> &'static [u8] {
    &AGENT_RELEASE_METADATA
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::agent_metadata::parse_metadata_bytes;

    #[test]
    fn embedded_metadata_parses_cleanly() {
        let meta = parse_metadata_bytes(&AGENT_RELEASE_METADATA)
            .expect("embedded agent metadata must parse");
        assert_eq!(meta.product, "kanade-agent");
        assert!(!meta.version.is_empty());
    }
}
