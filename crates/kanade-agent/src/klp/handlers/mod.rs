//! Method handlers for KLP, grouped by SPEC §2.12.5 namespace.
//!
//! Shipping today: [`system`] (handshake / ping / version /
//! log_tail) + [`state`] (snapshot / subscribe / unsubscribe +
//! `state.changed` push) + [`jobs`] (`jobs.list` read-only catalog;
//! the execute/progress/kill run half lands in a follow-up PR) +
//! [`maintenance`] (`maintenance.list` upcoming-fire preview;
//! `maintenance.defer` lands in a follow-up PR).
//! [`support`] carries the helpdesk unlock gate (`support.unlock` /
//! `.lock` / `.status`); `support.upload_diagnostics` lands in a
//! follow-up PR alongside it.

pub mod jobs;
pub mod maintenance;
pub mod notifications;
pub mod state;
pub mod support;
pub mod system;
