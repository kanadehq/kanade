//! Cross-platform KLP connection; the GUI is built separately on Windows.

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub mod klp_client;
