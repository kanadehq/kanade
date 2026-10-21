//! The wire form of a typed native-deployment command.
//!
//! A distinct type from [`Command`](super::Command) on purpose: it carries no
//! `shell`, `script`, `script_object` or `cwd` key at all, so an agent that
//! predates it fails to decode it as a `Command` (required fields are absent)
//! instead of running anything, and a typed command can never be read as a
//! shell command by any decoder. It is only ever valid inside the signed
//! `kanade.command.v2` envelope (see `verify_typed_envelope`); a bare or
//! unsigned `Command`, and the legacy signed form, cannot carry it.
//!
//! Building one is a pure, side-effect-free conversion and **not an
//! authorisation**: no delivery path publishes it and no agent runs it while
//! the shared approval / capability gates and native handlers are missing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::RunAs;
use crate::artifact::ArtifactSha256;
use crate::manifest::Manifest;
use crate::typed_operation::{
    DeployAuthorization, ExpectedArtifact, KIND_TYPED_OPERATION, ManifestExecute,
    TYPED_PAYLOAD_VERSION, TypedOperation, parse_typed_timeout,
};

/// `command_kind` of a typed command; the only value, and it is required.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypedCommandKind {
    TypedOperation,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TypedCommand {
    pub command_kind: TypedCommandKind,
    pub payload_version: u32,
    pub id: String,
    pub version: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_id: Option<String>,
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<DateTime<Utc>>,
    pub run_as: RunAs,
    pub operation: TypedOperation,
    pub deploy_authorization: DeployAuthorization,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(remote = "TypedCommand")]
struct TypedCommandFields {
    pub command_kind: TypedCommandKind,
    pub payload_version: u32,
    pub id: String,
    pub version: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_id: Option<String>,
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<DateTime<Utc>>,
    pub run_as: RunAs,
    pub operation: TypedOperation,
    pub deploy_authorization: DeployAuthorization,
}
impl<'de> Deserialize<'de> for TypedCommand {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        TypedCommandFields::deserialize(crate::strict::UniqueDeserializer(de))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TypedCommandError {
    #[error("the job is not a typed_operation job")]
    NotTyped,
    #[error("invalid typed job: {0}")]
    Job(String),
    #[error("typed command does not match the expected artifact: {0}")]
    Binding(String),
    #[error("invalid typed command: {0}")]
    Invalid(String),
}

/// Caller-supplied inputs of the conversion. Everything that must be trusted
/// comes from here (the artifact store, the plan), never from the job itself.
#[derive(Debug, Clone)]
pub struct TypedCommandRequest<'a> {
    pub request_id: String,
    pub exec_id: Option<String>,
    pub deadline_at: Option<DateTime<Utc>>,
    /// SHA-256 of the exact job manifest the plan approved.
    pub job_manifest_digest: ArtifactSha256,
    pub expected: ExpectedArtifact<'a>,
}

impl TypedCommand {
    pub fn validate(&self) -> Result<(), TypedCommandError> {
        let bad = |m: String| TypedCommandError::Invalid(m);
        if self.payload_version != TYPED_PAYLOAD_VERSION {
            return Err(bad(format!(
                "unsupported payload_version {}",
                self.payload_version
            )));
        }
        if self.id.is_empty() || self.version.is_empty() || self.request_id.is_empty() {
            return Err(bad("id, version and request_id must not be empty".into()));
        }
        if self.run_as != RunAs::System {
            return Err(bad("a typed operation runs as `system` only".into()));
        }
        let max = crate::typed_operation::MAX_TYPED_TIMEOUT.as_secs();
        if self.timeout_secs == 0 || self.timeout_secs > max {
            return Err(bad(format!("timeout_secs must be 1..={max}")));
        }
        self.deploy_authorization.validate().map_err(bad)?;
        self.operation.validate().map_err(bad)
    }

    /// Validated, side-effect-free Manifest → Command conversion for one
    /// target. Checks the whole job again and binds it to the externally
    /// expected artifact. It does not consult approval, capability or trust
    /// state, so a successful conversion is not permission to deliver or run.
    pub fn from_manifest(
        manifest: &Manifest,
        req: TypedCommandRequest<'_>,
    ) -> Result<Self, TypedCommandError> {
        let ManifestExecute::Typed(typed) = &manifest.execute else {
            return Err(TypedCommandError::NotTyped);
        };
        manifest.validate().map_err(TypedCommandError::Job)?;
        let auth = manifest
            .deploy_authorization
            .as_ref()
            .ok_or_else(|| TypedCommandError::Job("missing deploy_authorization".into()))?;
        typed
            .operation
            .verify_binding(&req.expected)
            .map_err(TypedCommandError::Binding)?;
        let timeout = parse_typed_timeout(&typed.timeout).map_err(TypedCommandError::Job)?;
        let cmd = TypedCommand {
            command_kind: TypedCommandKind::TypedOperation,
            payload_version: typed.payload_version,
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            request_id: req.request_id,
            exec_id: req.exec_id,
            timeout_secs: timeout.as_secs(),
            deadline_at: req.deadline_at,
            run_as: typed.run_as,
            operation: typed.operation.clone(),
            deploy_authorization: DeployAuthorization {
                plan_id: auth.plan_id.clone(),
                plan_digest: auth.plan_digest,
                manifest_digest: req.job_manifest_digest,
            },
        };
        cmd.validate()?;
        Ok(cmd)
    }
}

/// True when a decoded command object names a typed operation.
pub(crate) fn names_typed(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    obj.get("command_kind").and_then(|v| v.as_str()) == Some(KIND_TYPED_OPERATION)
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::typed_operation::fixtures::*;

    /// A valid typed backend command converted from the fixture job.
    pub(crate) fn typed_command() -> TypedCommand {
        let (id, integ, msha) = (identity(), integrity(), manifest_sha());
        let req = TypedCommandRequest {
            request_id: "req-1".into(),
            exec_id: Some("exec-1".into()),
            deadline_at: None,
            job_manifest_digest: ArtifactSha256::of(b"job manifest"),
            expected: ExpectedArtifact {
                identity: &id,
                integrity: &integ,
                manifest_sha256: &msha,
            },
        };
        TypedCommand::from_manifest(&backend_manifest(), req).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::typed_command;
    use super::*;
    use crate::artifact::ArtifactIntegrity;
    use crate::typed_operation::fixtures::*;
    use crate::wire::{Command, Shell};

    fn req<'a>(
        id: &'a crate::artifact::ArtifactIdentity,
        integ: &'a crate::artifact::ArtifactIntegrity,
        msha: &'a ArtifactSha256,
    ) -> TypedCommandRequest<'a> {
        TypedCommandRequest {
            request_id: "req-1".into(),
            exec_id: Some("exec-1".into()),
            deadline_at: None,
            job_manifest_digest: ArtifactSha256::of(b"job manifest"),
            expected: ExpectedArtifact {
                identity: id,
                integrity: integ,
                manifest_sha256: msha,
            },
        }
    }

    /// The decoder every agent shipped before typed commands existed: the
    /// required fields of `Command`, frozen here so the test cannot drift
    /// along with the current type.
    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct FrozenLegacyCommand {
        id: String,
        version: String,
        request_id: String,
        exec_id: Option<String>,
        shell: Shell,
        script: String,
        timeout_secs: u64,
        jitter_secs: Option<u64>,
    }

    #[test]
    fn conversion_copies_the_validated_job_and_binds_plan_and_job_digests() {
        let c = typed_command();
        assert_eq!(c.command_kind, TypedCommandKind::TypedOperation);
        assert_eq!(c.id, "deploy-backend");
        assert_eq!(c.timeout_secs, 600);
        assert_eq!(c.run_as, RunAs::System);
        assert_eq!(c.deploy_authorization.plan_id, "plan-1");
        assert_eq!(c.deploy_authorization.plan_digest, plan_digest());
        assert_eq!(
            c.deploy_authorization.manifest_digest,
            ArtifactSha256::of(b"job manifest")
        );
        c.validate().unwrap();
    }

    #[test]
    fn conversion_refuses_shell_jobs_and_every_binding_mismatch() {
        let (id, integ, msha) = (identity(), integrity(), manifest_sha());
        let shell: Manifest = crate::strict::from_yaml_str(
            "id: j\nversion: 1\nexecute:\n  shell: sh\n  script: x\n  timeout: 5s\n",
        )
        .unwrap();
        assert_eq!(
            TypedCommand::from_manifest(&shell, req(&id, &integ, &msha)).unwrap_err(),
            TypedCommandError::NotTyped
        );
        let m = backend_manifest();
        let wrong_integ = ArtifactIntegrity::of(b"x");
        assert!(matches!(
            TypedCommand::from_manifest(&m, req(&id, &wrong_integ, &msha)),
            Err(TypedCommandError::Binding(_))
        ));
        let wrong_msha = ArtifactSha256::of(b"x");
        assert!(matches!(
            TypedCommand::from_manifest(&m, req(&id, &integ, &wrong_msha)),
            Err(TypedCommandError::Binding(_))
        ));
        // A job that no longer validates is not converted either.
        let mut bad = backend_manifest();
        bad.require_approval = false;
        assert!(matches!(
            TypedCommand::from_manifest(&bad, req(&id, &integ, &msha)),
            Err(TypedCommandError::Job(_))
        ));
    }

    #[test]
    fn typed_wire_form_has_no_shell_keys_and_a_literal_discriminator() {
        let v = serde_json::to_value(typed_command()).unwrap();
        for k in [
            "shell",
            "script",
            "script_object",
            "script_object_sha256",
            "cwd",
        ] {
            assert!(v.get(k).is_none(), "{k} must not exist on a typed command");
        }
        assert_eq!(v["command_kind"], "typed_operation");
        assert_eq!(v["payload_version"], 1);
        let back: TypedCommand = serde_json::from_value(v).unwrap();
        assert_eq!(back, typed_command());
    }

    #[test]
    fn an_agent_that_predates_typed_commands_cannot_decode_one() {
        let json = serde_json::to_string(&typed_command()).unwrap();
        let err = serde_json::from_str::<FrozenLegacyCommand>(&json).unwrap_err();
        assert!(err.to_string().contains("missing field"), "{err}");
        // The current shell decoder refuses it too (no shell fallback).
        assert!(serde_json::from_str::<Command>(&json).is_err());
    }

    #[test]
    fn shell_command_never_accepts_a_typed_or_unknown_kind() {
        let shell = r#""id":"j","version":"1","request_id":"r","exec_id":null,"shell":"sh","script":"echo","timeout_secs":5,"jitter_secs":null"#;
        // Historical bytes (no discriminator) decode, and serialise back with none.
        let c: Command = serde_json::from_str(&format!("{{{shell}}}")).unwrap();
        assert!(c.command_kind.is_shell());
        assert!(!serde_json::to_string(&c).unwrap().contains("command_kind"));
        let c: Command =
            serde_json::from_str(&format!("{{\"command_kind\":\"shell\",{shell}}}")).unwrap();
        assert!(c.command_kind.is_shell());
        // Mixed typed + shell data must not be run as shell.
        for kind in [
            "\"typed_operation\"",
            "\"future\"",
            "null",
            "7",
            "\"Shell\"",
        ] {
            let json = format!("{{\"command_kind\":{kind},{shell}}}");
            assert!(serde_json::from_str::<Command>(&json).is_err(), "{kind}");
        }
    }

    #[test]
    fn strict_decode_rejects_unknown_mixed_and_unsupported_payloads() {
        let base = serde_json::to_value(typed_command()).unwrap();
        let mut cases = Vec::new();
        for (k, v) in [
            ("shell", serde_json::json!("sh")),
            ("script", serde_json::json!("rm -rf /")),
            ("surprise", serde_json::json!(1)),
            ("payload_version", serde_json::json!(2)),
            ("command_kind", serde_json::json!("shell")),
            ("command_kind", serde_json::Value::Null),
            ("run_as", serde_json::json!("user")),
            ("timeout_secs", serde_json::json!(0)),
            ("timeout_secs", serde_json::json!(100_000)),
        ] {
            let mut o = base.clone();
            o[k] = v.clone();
            cases.push((format!("{k}={v}"), o));
        }
        for (name, o) in cases {
            let parsed = serde_json::from_value::<TypedCommand>(o);
            assert!(
                parsed.map_or(true, |c| c.validate().is_err()),
                "{name} was accepted"
            );
        }
        let mut o = base;
        o["operation"]["type"] = serde_json::json!("deploy_all");
        assert!(serde_json::from_value::<TypedCommand>(o).is_err());
    }

    #[test]
    fn missing_authorization_or_malformed_digests_are_rejected() {
        let base = serde_json::to_value(typed_command()).unwrap();
        let mut o = base.clone();
        o.as_object_mut().unwrap().remove("deploy_authorization");
        assert!(serde_json::from_value::<TypedCommand>(o).is_err());
        let mut o = base;
        o["deploy_authorization"]["manifest_digest"] = serde_json::json!("ABC");
        assert!(serde_json::from_value::<TypedCommand>(o).is_err());
    }
}
