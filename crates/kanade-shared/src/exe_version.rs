//! Extract the embedded `ProductVersion` / `FileVersion` string from
//! a Windows PE binary's `VERSIONINFO` resource — the same data
//! File Explorer's Details tab shows.
//!
//! Used by:
//!   * `kanade-backend`'s `POST /api/agents/publish` — auto-derives
//!     the Object-Store key from the uploaded bytes, so the operator
//!     can't typo a label that disagrees with the binary (the failure
//!     mode that caused the "rollout to 1.0.0 → endless self-update"
//!     incident on v0.13.0).
//!   * `kanade agent publish` on the CLI — replaces the spawn-based
//!     `--version` probe (which only worked on hosts that could
//!     execute the binary).
//!
//! Pure-`pelite`, no spawn / no OS-specific deps — works from any
//! host (Linux operator uploading a Windows .exe is fine).

use pelite::pe::{Pe, PeFile};

/// Read the `ProductVersion` (falling back to `FileVersion`) from
/// the VS_VERSIONINFO resource of a Windows PE. Returns `None` when:
///   * the bytes aren't a valid PE32 / PE32+,
///   * there's no `.rsrc` section,
///   * there's no `VS_VERSIONINFO` resource, or
///   * neither version string is set.
pub fn extract_pe_version(bytes: &[u8]) -> Option<String> {
    let pe = PeFile::from_bytes(bytes).ok()?;
    let resources = pe.resources().ok()?;
    let version_info = resources.version_info().ok()?;

    // VersionInfo carries one or more language-keyed string tables.
    // ProductVersion / FileVersion are 4 standard members of each.
    // Prefer the first language that actually has the string set.
    for language in version_info.translation() {
        if let Some(s) = version_info.value(*language, "ProductVersion")
            && !s.is_empty()
        {
            return Some(normalise(&s));
        }
        if let Some(s) = version_info.value(*language, "FileVersion")
            && !s.is_empty()
        {
            return Some(normalise(&s));
        }
    }
    None
}

/// Trim trailing nulls + whitespace. `winres`-written strings often
/// have a single embedded NUL terminator the resource compiler keeps
/// in the payload; pelite returns it raw.
fn normalise(s: &str) -> String {
    s.trim_end_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string()
}
