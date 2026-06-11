//! Method handlers for KLP, grouped by SPEC §2.12.5 namespace.
//!
//! Shipping today: [`system`] (handshake / ping / version /
//! log_tail) + [`state`] (snapshot / subscribe / unsubscribe +
//! `state.changed` push) + [`jobs`] (`jobs.list` read-only catalog;
//! the execute/progress/kill run half lands in a follow-up PR) +
//! [`maintenance`] (`maintenance.list` upcoming-fire preview;
//! `maintenance.defer` lands in a follow-up PR).
//! Remaining namespaces (notifications, support) land in follow-up
//! PRs; each adds one sibling module and routes for it in
//! [`super::dispatcher`].

pub mod jobs;
pub mod maintenance;
pub mod notifications;
pub mod state;
pub mod system;
