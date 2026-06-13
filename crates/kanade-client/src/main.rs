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
//! Sprint 8 ships only the Windows skeleton (Tauri 2.x + KLP
//! Named Pipe client + handshake + ping). On non-Windows targets
//! (CI cross-checks, dev curiosity) the binary is a stub that
//! prints + exits — Tauri + the Pipe transport are both
//! Windows-bound for now. Linux UDS support lands in a follow-up
//! PR and will widen the cfg gate.

#[cfg(target_os = "windows")]
mod app;
#[cfg(target_os = "windows")]
mod klp_client;

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

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("kanade-client is Windows-only in this build (Linux UDS variant is a follow-up PR).");
    std::process::exit(1);
}
