//! KLP (Kanade Local Protocol) shared types — SPEC §2.12.
//!
//! KLP is the IPC protocol between the Client App (Tauri) and the
//! agent. The shared types in this module are consumed by both
//! sides (`kanade-agent::klp` and the eventual `kanade-client`)
//! and, in time, exported to TypeScript for the Tauri WebView via
//! ts-rs.
//!
//! Module layout matches SPEC §2.12.5's method namespace:
//!
//! - [`envelope`] / [`error`] — JSON-RPC 2.0 framing types
//!   ([`envelope::RpcRequest`], [`envelope::RpcNotification`],
//!   [`envelope::RpcResponse`]) plus the error model
//!   ([`error::RpcError`], [`error::ErrorKind`]).
//! - [`method`] — string constants for every v1 method name.
//! - [`handshake`], [`system`] — `system.*` methods.
//! - [`state`] — `state.snapshot` / `state.subscribe` /
//!   `state.changed`.
//! - [`notifications`] — `notifications.list` / `subscribe` /
//!   `ack` + `notifications.new` push.
//! - [`jobs`] — `jobs.list` / `execute` / `subscribe` / `kill` +
//!   `jobs.progress` push.
//! - [`support`] — `support.upload_diagnostics`.
//! - [`maintenance`] — `maintenance.list` / `maintenance.defer`.
//!
//! ts-rs export (SPEC §2.12.11) is deferred until the
//! `kanade-client` crate lands — until then there's no TS consumer
//! to import the bindings, and pulling the ts-rs dependency in
//! without a build hook to generate the `bindings/ipc/*.ts` files
//! would be dead code.

pub mod envelope;
pub mod error;
pub mod handshake;
pub mod jobs;
pub mod maintenance;
pub mod method;
pub mod notifications;
pub mod state;
pub mod support;
pub mod system;
