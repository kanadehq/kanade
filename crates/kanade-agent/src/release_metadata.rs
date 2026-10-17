//! Release metadata embedded in the agent binary.
//!
//! The build script renders `{"product":"kanade-agent","version":…,
//! "platform":…}` in the envelope defined by
//! `kanade_shared::agent_metadata`; this module links it into a dedicated
//! section (`.kanade` in PE / ELF, `__DATA,__kanade` in Mach-O) that the
//! backend's agent-publish admission reads. Only this crate embeds it, so
//! no other kanade binary can identify itself as the agent.
//!
//! `main` reads the static through [`ensure_embedded`] so neither LTO nor
//! the linker's section GC (`--gc-sections`, `-dead_strip`) can drop it;
//! `strip` leaves allocated sections alone, and the macOS code signature
//! is applied afterwards without touching section contents.

const METADATA_LEN: usize = include_bytes!(concat!(env!("OUT_DIR"), "/agent_metadata.bin")).len();

#[used]
#[cfg_attr(
    any(target_os = "windows", target_os = "linux"),
    unsafe(link_section = ".kanade")
)]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__kanade"))]
static AGENT_RELEASE_METADATA: [u8; METADATA_LEN] =
    *include_bytes!(concat!(env!("OUT_DIR"), "/agent_metadata.bin"));

/// Opaque read of the metadata so the section stays reachable.
#[inline(never)]
pub fn ensure_embedded() -> &'static [u8] {
    &AGENT_RELEASE_METADATA
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::agent_metadata::{
        AGENT_PRODUCT_NAME, admit_agent_binary, parse_metadata_bytes,
    };

    #[test]
    fn embedded_metadata_matches_the_crate() {
        let meta = parse_metadata_bytes(ensure_embedded()).expect("embedded metadata parses");
        assert_eq!(meta.product, AGENT_PRODUCT_NAME);
        assert_eq!(meta.version, env!("CARGO_PKG_VERSION"));
    }

    /// The linked executable (this test binary) is admitted by the same check
    /// the backend runs, so the section name / format survive the real linker.
    #[test]
    fn embedded_metadata_is_admitted() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let admitted = admit_agent_binary(&exe, Some(env!("CARGO_PKG_VERSION")))
            .expect("linked binary must pass backend admission");
        assert_eq!(admitted.version, env!("CARGO_PKG_VERSION"));
    }
}
