// Hide the extra console window on Windows release builds — this is a
// GUI app, so a GUI-subsystem link keeps the black console (with its
// raw ANSI tracing output) from popping up behind the WebView. Debug
// builds keep the console subsystem so `tracing` logs stay visible
// during development. The Tauri CLI sets this automatically; we build
// the client with plain `cargo build`, so we set it by hand.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

//! Entry point for the Kanade Client App.
//!
//! Windows and Apple Silicon macOS 14+ run the production Tauri UI.
//! Other targets retain an exit-fast shim; KLP remains cross-platform.

#[cfg(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
))]
mod app;
#[cfg(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
))]
use kanade_client::klp_client;

#[cfg(target_os = "windows")]
fn main() {
    // GUI-subsystem release builds have no console, so plain `fmt()` writes
    // tracing into the void — write to a file instead so the client's logs
    // (KLP connect, emergency-toast path, …) are inspectable like the agent's.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "kanade_client=info".into());
    let log_path = r"C:\ProgramData\Kanade\logs\client.log";
    // Create the logs dir first (fresh install won't have it) — otherwise the
    // open below fails into the console-less fallback and nothing is logged.
    // Mirrors the agent/backend, which create_dir_all before opening.
    if let Some(dir) = std::path::Path::new(log_path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        // No writable log dir (non-standard install / CI) — fall back to the
        // (console-less but harmless) default rather than failing to start.
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
    app::run();
}

#[cfg(not(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
)))]
fn main() {
    eprintln!("kanade-client GUI requires Windows or Apple Silicon macOS 14+ in this build.");
    std::process::exit(1);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "kanade_client=info".into());
    match kanade_client::macos_storage::open_log() {
        Ok(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        Err(error) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            tracing::warn!(%error, "private log unavailable; using stderr");
        }
    }
    app::run();
}
