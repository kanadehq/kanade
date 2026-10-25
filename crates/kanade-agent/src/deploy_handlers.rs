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

/// Handlers whose installer code exists and is tested in this build, but
/// that are **not** wired to any delivery path (SPEC §2.13 Task 5, stage 5a:
/// the macOS arm64 admin CLI). This is distinct from [`implemented`]: the
/// heartbeat reports only the latter, so no remote deploy capability is
/// advertised until the signed-command / replay / approval / result path is
/// integrated and a later change moves the entry across.
#[allow(dead_code)]
pub fn tested_but_not_dispatch_ready() -> Vec<DeployHandler> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        use kanade_shared::artifact::{TargetArch, TargetOs};
        use kanade_shared::wire::{OPERATION_DEPLOY_CLI, TYPED_DEPLOY_CONTRACT_V1};
        vec![DeployHandler::new(
            TYPED_DEPLOY_CONTRACT_V1,
            OPERATION_DEPLOY_CLI,
            TargetOs::Macos,
            TargetArch::Aarch64,
        )]
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        Vec::new()
    }
}

/// End-to-end deploy readiness. Always `false` in this build.
#[allow(dead_code)]
pub const END_TO_END_DEPLOY_READY: bool = false;

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

    #[test]
    fn tested_installer_support_is_not_advertised_or_dispatch_ready() {
        const { assert!(!END_TO_END_DEPLOY_READY) };
        for h in tested_but_not_dispatch_ready() {
            assert!(!implemented().contains(&h));
            assert!(!capabilities().handlers.contains(&h));
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(tested_but_not_dispatch_ready().len(), 1);
    }
}
