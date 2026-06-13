//! KLP (Kanade Local Protocol) agent-side implementation —
//! SPEC §2.12.
//!
//! Module layout matches SPEC §2.12.13's "実装責務分担" table:
//!
//! - [`server`] — Windows Named Pipe listener + per-connection
//!   read/write split (reader task + writer task fed via mpsc).
//! - [`framing`] — SPEC §2.12.2 length-prefixed JSON codec.
//! - [`auth`] — OS token → SID/Session-id derivation
//!   (`GetNamedPipeClientProcessId` chain).
//! - [`security`] — Named Pipe SECURITY_DESCRIPTOR construction
//!   (SPEC §2.12.1: Authenticated Users RW, deny Anonymous).
//! - [`connection`] — per-connection state (handshake gate,
//!   peer credentials, subscription registry, push channel).
//! - [`dispatcher`] — method routing + envelope assembly.
//! - [`state`] — endpoint state evaluator (background task);
//!   feeds the `state.snapshot` cache and the `state.changed`
//!   push stream.
//! - [`subscriptions`] — per-connection registry of active push
//!   forwarder tasks; aborts on disconnect.
//! - [`handlers`] — per-namespace method implementations
//!   (`system.*` + `state.*` today; notifications/jobs/support/
//!   maintenance land in follow-up PRs).
//!
//! Wire types are owned by [`kanade_shared::ipc`]; the agent side
//! consumes them without re-exporting.
//!
//! # Platform gate
//!
//! The whole module is Windows-only today (SPEC §2.12.1): the
//! listener is a Named Pipe and peer auth walks the Win32 token
//! chain. `main.rs` keeps `mod klp;` behind
//! `#[cfg(target_os = "windows")]`, so nothing below is ever
//! compiled on Linux/macOS. The `compile_error!` here is
//! defense-in-depth: if a future refactor accidentally un-gates
//! the module on a non-Windows target, the build fails loudly at
//! compile time instead of reaching [`auth::resolve_peer`] and
//! panicking at runtime. When the Linux UDS path (SPEC §2.12.4's
//! `SO_PEERCRED`) actually lands, drop this guard and provide the
//! real non-Windows implementations.
//!
//! The submodules are `#[cfg(target_os = "windows")]`-gated too,
//! so on an accidental non-Windows un-gate the `compile_error!`
//! below is the *only* diagnostic — the compiler never tries to
//! parse the Win32-only submodules and bury it under
//! `unresolved import` noise.

#[cfg(not(target_os = "windows"))]
compile_error!(
    "crate::klp is Windows-only (SPEC §2.12.1: Named Pipe + Win32 peer auth). \
     `mod klp;` in main.rs must stay `#[cfg(target_os = \"windows\")]`-gated. \
     If you intentionally enabled it on a non-Windows target, implement the UDS \
     listener + `SO_PEERCRED` peer auth (SPEC §2.12.4) before removing this guard."
);

#[cfg(target_os = "windows")]
pub mod auth;
#[cfg(target_os = "windows")]
pub mod connection;
#[cfg(target_os = "windows")]
pub mod dispatcher;
#[cfg(target_os = "windows")]
pub mod emergency_notify;
#[cfg(target_os = "windows")]
pub mod framing;
#[cfg(target_os = "windows")]
pub mod handlers;
#[cfg(target_os = "windows")]
pub mod notify_bus;
#[cfg(target_os = "windows")]
pub mod security;
#[cfg(target_os = "windows")]
pub mod server;
#[cfg(target_os = "windows")]
pub mod state;
#[cfg(target_os = "windows")]
pub mod subscriptions;
