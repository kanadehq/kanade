//! Method handlers for KLP, grouped by SPEC §2.12.5 namespace.
//!
//! This PR ships only [`system`] (with handshake + ping). Other
//! namespaces (state, notifications, jobs, support, maintenance)
//! land in follow-up PRs; each follow-up adds one sibling module
//! and routes for it in [`super::dispatcher`].

pub mod system;
