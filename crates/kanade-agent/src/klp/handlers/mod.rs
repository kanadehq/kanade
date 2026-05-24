//! Method handlers for KLP, grouped by SPEC §2.12.5 namespace.
//!
//! Shipping today: [`system`] (handshake / ping / version /
//! log_tail) + [`state`] (snapshot / subscribe / unsubscribe +
//! `state.changed` push). Remaining namespaces (notifications,
//! jobs, support, maintenance) land in follow-up PRs; each
//! follow-up adds one sibling module and routes for it in
//! [`super::dispatcher`].

pub mod state;
pub mod system;
