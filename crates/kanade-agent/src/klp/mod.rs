//! KLP (Kanade Local Protocol) agent-side implementation —
//! SPEC §2.12.
//!
//! Module layout matches SPEC §2.12.13's "実装責務分担" table:
//!
//! - [`server`] — Windows Named Pipe listener + per-connection
//!   read/write loop.
//! - [`framing`] — SPEC §2.12.2 length-prefixed JSON codec.
//! - [`auth`] — OS token → SID/Session-id derivation
//!   (`GetNamedPipeClientProcessId` chain).
//! - [`security`] — Named Pipe SECURITY_DESCRIPTOR construction
//!   (SPEC §2.12.1: Authenticated Users RW, deny Anonymous).
//! - [`connection`] — per-connection state (handshake gate,
//!   peer credentials).
//! - [`dispatcher`] — method routing + envelope assembly.
//! - [`handlers`] — per-namespace method implementations
//!   (`system.*` in this PR; state/notifications/jobs/support/
//!   maintenance land in follow-up PRs).
//!
//! Wire types are owned by [`kanade_shared::ipc`]; the agent side
//! consumes them without re-exporting.

pub mod auth;
pub mod connection;
pub mod dispatcher;
pub mod framing;
pub mod handlers;
pub mod security;
pub mod server;
