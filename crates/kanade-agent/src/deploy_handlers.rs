//! Registry of typed native-deployment handlers this agent build can run.
//!
//! The heartbeat's `deploy_capabilities` is generated from [`implemented`] and
//! from nothing else, so an advertised capability always has a working handler
//! behind it. No native handler exists yet (SPEC §2.13 Task 5), so the
//! registry is empty and the agent reports an explicit "supports nothing
//! typed" — distinct from the `None` an old agent or a ping reply sends.
//! There is deliberately no switch, config value or feature flag that adds an
//! entry: a handler is registered by the change that implements and verifies
//! it.

use kanade_shared::wire::{DeployCapabilities, DeployHandler};

/// Handlers with a real, verified implementation in this build.
fn implemented() -> &'static [DeployHandler] {
    &[]
}

/// The capability report for the heartbeat.
pub fn capabilities() -> DeployCapabilities {
    DeployCapabilities::from_handlers(implemented())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_handler_is_implemented_so_nothing_is_advertised() {
        let caps = capabilities();
        assert!(
            caps.handlers.is_empty(),
            "advertised a handler without an implementation"
        );
        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(json["handlers"], serde_json::json!([]));
    }
}
