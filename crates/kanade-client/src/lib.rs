//! Cross-platform KLP connection and pure Client policies.
//! The GUI supports Windows and Apple Silicon macOS 14+.

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub mod klp_client;

/// Per-user Unix data layout and OS account/storage boundaries (no GUI backend).
pub mod paths;
/// Conservative automatic notification routing (never KLP authorization).
pub mod session;
pub mod startup;
pub mod window_size;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod macos_storage;
