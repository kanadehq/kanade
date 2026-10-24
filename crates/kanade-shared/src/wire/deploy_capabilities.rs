//! Structured typed-deployment capability facts an agent reports in its
//! heartbeat (SPEC §2.13.6.3).
//!
//! A capability is a *claim that a working handler exists*, never a
//! description of a wish. The agent builds the list from its handler registry
//! ([`DeployCapabilities::from_handlers`]); a build with no handler reports an
//! explicit empty list. Nothing in this module grants permission to deploy: it
//! is evidence the control-plane preflight reads, alongside the plan approval
//! and the command-signing facts.
//!
//! Three states, the same convention as `command_keys` / `command_protocols`:
//!
//! * `None` on the heartbeat — an older agent, or a ping reply: unknown.
//! * `Some` with no handlers — reporting, and supports no typed operation.
//! * `Some` with handlers — the exact (contract, operation, os, arch) tuples
//!   it can run right now.
//!
//! Wire strings are deliberately not enums: an unknown token from a newer
//! agent must still decode (a failed decode would drop the whole heartbeat)
//! and is then ignored by [`DeployHandler::known`], i.e. fails closed.

use serde::{Deserialize, Serialize};

use crate::artifact::{TargetArch, TargetOs};

/// The only typed-deployment contract this control plane understands.
pub const TYPED_DEPLOY_CONTRACT_V1: &str = "typed_deploy_v1";
/// Schema version of [`DeployCapabilities`] itself.
pub const DEPLOY_CAPABILITIES_VERSION: u32 = 1;

/// The three fixed typed operations (`TypedOperation` `type` tokens).
pub const OPERATION_DEPLOY_BACKEND: &str = "deploy_backend";
pub const OPERATION_DEPLOY_CLIENT: &str = "deploy_client";
pub const OPERATION_DEPLOY_CLI: &str = "deploy_cli";

/// One operation an agent can run on its own platform.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeployHandler {
    pub contract: String,
    pub operation: String,
    pub os: String,
    pub arch: String,
}

impl DeployHandler {
    pub fn new(contract: &str, operation: &str, os: TargetOs, arch: TargetArch) -> Self {
        Self {
            contract: contract.to_owned(),
            operation: operation.to_owned(),
            os: os.as_str().to_owned(),
            arch: arch.as_str().to_owned(),
        }
    }

    /// The handler as a typed tuple, or `None` when any component is a token
    /// this build does not know (never guessed at, never matched).
    pub fn known(&self) -> Option<(&'static str, TargetOs, TargetArch)> {
        let operation = [
            OPERATION_DEPLOY_BACKEND,
            OPERATION_DEPLOY_CLIENT,
            OPERATION_DEPLOY_CLI,
        ]
        .into_iter()
        .find(|o| *o == self.operation)?;
        if self.contract != TYPED_DEPLOY_CONTRACT_V1 {
            return None;
        }
        Some((
            operation,
            self.os.parse::<TargetOs>().ok()?,
            self.arch.parse::<TargetArch>().ok()?,
        ))
    }
}

/// `Heartbeat.deploy_capabilities`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DeployCapabilities {
    pub schema_version: u32,
    pub handlers: Vec<DeployHandler>,
}

impl DeployCapabilities {
    /// Build the report from the handlers that actually exist in this build.
    /// An empty slice is the honest answer today.
    pub fn from_handlers(handlers: &[DeployHandler]) -> Self {
        Self {
            schema_version: DEPLOY_CAPABILITIES_VERSION,
            handlers: handlers.to_vec(),
        }
    }

    /// True when a *known* handler for exactly this (operation, os, arch)
    /// is reported. Unknown schema versions and tokens never match.
    pub fn supports(&self, operation: &str, os: TargetOs, arch: TargetArch) -> bool {
        self.schema_version == DEPLOY_CAPABILITIES_VERSION
            && self
                .handlers
                .iter()
                .filter_map(DeployHandler::known)
                .any(|(op, o, a)| op == operation && o == os && a == arch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> DeployHandler {
        DeployHandler::new(
            TYPED_DEPLOY_CONTRACT_V1,
            OPERATION_DEPLOY_CLI,
            TargetOs::Linux,
            TargetArch::X86_64,
        )
    }

    #[test]
    fn empty_report_supports_nothing_and_roundtrips_as_empty() {
        let caps = DeployCapabilities::from_handlers(&[]);
        let json = serde_json::to_string(&caps).unwrap();
        assert_eq!(json, r#"{"schema_version":1,"handlers":[]}"#);
        let back: DeployCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back, caps);
        assert!(!back.supports(OPERATION_DEPLOY_CLI, TargetOs::Linux, TargetArch::X86_64));
    }

    #[test]
    fn supports_requires_the_exact_tuple() {
        let caps = DeployCapabilities::from_handlers(&[handler()]);
        assert!(caps.supports(OPERATION_DEPLOY_CLI, TargetOs::Linux, TargetArch::X86_64));
        assert!(!caps.supports(OPERATION_DEPLOY_CLI, TargetOs::Linux, TargetArch::Aarch64));
        assert!(!caps.supports(OPERATION_DEPLOY_CLI, TargetOs::Macos, TargetArch::X86_64));
        assert!(!caps.supports(OPERATION_DEPLOY_CLIENT, TargetOs::Linux, TargetArch::X86_64));
    }

    #[test]
    fn unknown_tokens_and_versions_decode_but_never_match() {
        let future: DeployCapabilities = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "handlers": [
                {"contract": "typed_deploy_v2", "operation": "deploy_cli", "os": "linux", "arch": "x86_64"},
                {"contract": "typed_deploy_v1", "operation": "deploy_toaster", "os": "linux", "arch": "x86_64"},
                {"contract": "typed_deploy_v1", "operation": "deploy_cli", "os": "plan9", "arch": "x86_64"}
            ]
        }))
        .unwrap();
        assert!(!future.supports(OPERATION_DEPLOY_CLI, TargetOs::Linux, TargetArch::X86_64));
        let newer = DeployCapabilities {
            schema_version: 2,
            handlers: vec![handler()],
        };
        assert!(!newer.supports(OPERATION_DEPLOY_CLI, TargetOs::Linux, TargetArch::X86_64));
    }
}
