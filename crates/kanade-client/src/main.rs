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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kanade_client=info".into()),
        )
        .init();
    app::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("kanade-client is Windows-only in this build (Linux UDS variant is a follow-up PR).");
    std::process::exit(1);
}
