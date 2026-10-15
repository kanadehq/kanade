//! Cross-platform KLP connection and pure Client policies.
//! The GUI remains Windows-only; Unix OS adapters are not implemented.

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub mod klp_client;

/// Per-user Unix data layout and OS account/storage boundaries (no GUI backend).
pub mod paths;
/// Conservative automatic notification routing (never KLP authorization).
pub mod session;
pub mod startup;
pub mod window_size;
