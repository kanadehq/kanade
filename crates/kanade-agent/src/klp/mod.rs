//! KLP (Kanade Local Protocol) agent-side implementation —
//! SPEC §2.12.
//!
//! Module layout matches SPEC §2.12.13's "実装責務分担" table:
//!
//! - [`server`] — listener + per-connection read/write split
//!   (reader task + writer task fed via mpsc). Windows Named Pipe or
//!   Unix domain socket behind one shared connection core.
//! - [`framing`] — SPEC §2.12.2 length-prefixed JSON codec.
//! - [`auth`] — platform-neutral `PeerIdentity` plus the OS peer
//!   lookup: Windows token chain, or Unix peer credentials with the
//!   connect-time admission rules (SPEC §2.12.4.1).
//! - [`security`] — Named Pipe SECURITY_DESCRIPTOR construction
//!   (Windows only; SPEC §2.12.1: Authenticated Users RW, deny Anonymous).
//! - [`connection`] — per-connection state (handshake gate,
//!   peer identity, subscription registry, push channel).
//! - [`dispatcher`] — method routing + envelope assembly.
//! - [`state`] — endpoint state evaluator (background task);
//!   feeds the `state.snapshot` cache and the `state.changed`
//!   push stream.
//! - [`subscriptions`] — per-connection registry of active push
//!   forwarder tasks; aborts on disconnect.
//! - [`handlers`] — per-namespace method implementations.
//!
//! Wire types are owned by [`kanade_shared::ipc`]; the agent side
//! consumes them without re-exporting.
//!
//! # Platform gate
//!
//! Everything compiles on Windows, Linux and macOS except
//! [`security`] and [`emergency_notify`] (Win32 pipe SD / native
//! toast launching, Windows only). The `compile_error!` below fires
//! on any other target, which has no listener.

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
compile_error!(
    "crate::klp has no listener for this target (SPEC §2.12.1 defines Named Pipe, \
     Linux UDS and macOS UDS only)."
);

pub mod auth;
pub mod connection;
pub mod dispatcher;
#[cfg(target_os = "windows")]
pub mod emergency_notify;
pub mod framing;
pub mod handlers;
pub mod notify_bus;
#[cfg(target_os = "windows")]
pub mod security;
pub mod server;
pub mod state;
pub mod subscriptions;
pub mod unlock;
