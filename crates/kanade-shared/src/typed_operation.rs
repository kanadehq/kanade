//! Typed native-deployment job contract (SPEC §2.13.6.2, migration stage 3).
//!
//! This module defines the **schema, validation and conversion** of a typed
//! operation only. Nothing here authorises, publishes, dispatches or executes
//! a deployment: the shared approval / capability gates (Task 4) and the
//! native handlers (Task 5) do not exist yet, so every delivery path rejects
//! a typed job with [`TYPED_DEPLOY_DISABLED`]. Accepting the schema is not
//! deployment authorisation.
//!
//! A stored job's `execute` block is a compatible union ([`ManifestExecute`]):
//!
//! * no `kind` key → the historical shell form ([`Execute`]), byte-for-byte
//!   the record every existing job already is;
//! * `kind: typed_operation` → [`TypedExecute`]: no shell / script / cwd
//!   fields, a `payload_version`, and one fixed, structured [`TypedOperation`].
//!
//! The two forms never mix, and a malformed typed request is an error, never a
//! shell job: a `kind` that is null, unknown, or paired with the other form's
//! fields is rejected rather than interpreted by falling back to shell.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::artifact::{
    ArtifactIdentity, ArtifactIntegrity, ArtifactRole, ArtifactSha256, TargetOs,
};
use crate::manifest::{Execute, ExecuteShell};
use crate::wire::RunAs;

/// `execute.kind` / `command_kind` value of a typed native operation.
pub const KIND_TYPED_OPERATION: &str = "typed_operation";
/// `execute.kind` value naming the historical shell form explicitly.
pub const KIND_SHELL: &str = "shell";
/// The only typed payload version this build understands.
pub const TYPED_PAYLOAD_VERSION: u32 = 1;
/// Longest accepted typed-operation timeout (SPEC: an upper bound is checked).
pub const MAX_TYPED_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Longest accepted plan identifier.
pub const MAX_PLAN_ID_LEN: usize = 128;

/// The refusal every delivery / execution entry point returns until the shared
/// approval and capability gates and the native handlers exist.
pub const TYPED_DEPLOY_DISABLED: &str = "typed native deployment is not enabled: the shared plan \
approval and capability gates and the native handlers are not implemented, so a typed_operation \
job is accepted as a definition but is never published, dispatched or executed";

/// A typed job reached a path that can only run shell jobs, or a delivery path
/// while typed deployment is disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{}", TYPED_DEPLOY_DISABLED)]
pub struct TypedDeployDisabled;

/// Fixed, role-specific operation. Adjacently tagged: `{type, spec}`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", content = "spec", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum TypedOperation {
    DeployBackend(DeployBackendSpec),
    DeployClient(DeployClientSpec),
    DeployCli(DeployCliSpec),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartPolicy {
    Preserve,
    Never,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FirewallPolicy {
    Preserve,
    NoChange,
    ManagedPort(u16),
}

/// Client opt-out handling. Only `respect` exists: forcing past a user's
/// opt-out is not a deploy option.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OptOutPolicy {
    Respect,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeployBackendSpec {
    pub artifact_identity: ArtifactIdentity,
    pub package_object_key: String,
    pub artifact_sha256: ArtifactSha256,
    pub manifest_sha256: ArtifactSha256,
    pub preserve_config: bool,
    pub wipe_db: bool,
    pub require_db_backup: bool,
    pub start_policy: StartPolicy,
    #[serde(default)]
    pub repair_service: bool,
    /// Singleton-map form (`firewall: {managed_port: 8443}`) in YAML, as for
    /// the schedule `when` enum.
    #[serde(with = "serde_yaml::with::singleton_map")]
    pub firewall: FirewallPolicy,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeployClientSpec {
    pub artifact_identity: ArtifactIdentity,
    pub package_object_key: String,
    pub artifact_sha256: ArtifactSha256,
    pub manifest_sha256: ArtifactSha256,
    pub opt_out_policy: OptOutPolicy,
    pub restart_gui_session_only: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeployCliSpec {
    pub artifact_identity: ArtifactIdentity,
    pub package_object_key: String,
    pub artifact_sha256: ArtifactSha256,
    pub manifest_sha256: ArtifactSha256,
    /// Explicit adoption of the fixed macOS location only; never a path.
    #[serde(default)]
    pub adopt_existing_cli: bool,
}

/// The externally expected immutable artifact a typed operation must name.
/// Supplied by the caller from the artifact store / plan, never read back from
/// the job being checked.
#[derive(Debug, Clone, Copy)]
pub struct ExpectedArtifact<'a> {
    pub identity: &'a ArtifactIdentity,
    pub integrity: &'a ArtifactIntegrity,
    /// SHA-256 of the exact signed `package.manifest.json` bytes (distinct from
    /// the job manifest digest).
    pub manifest_sha256: &'a ArtifactSha256,
}

impl TypedOperation {
    pub fn artifact_identity(&self) -> &ArtifactIdentity {
        match self {
            Self::DeployBackend(s) => &s.artifact_identity,
            Self::DeployClient(s) => &s.artifact_identity,
            Self::DeployCli(s) => &s.artifact_identity,
        }
    }

    pub fn package_object_key(&self) -> &str {
        match self {
            Self::DeployBackend(s) => &s.package_object_key,
            Self::DeployClient(s) => &s.package_object_key,
            Self::DeployCli(s) => &s.package_object_key,
        }
    }

    pub fn artifact_sha256(&self) -> &ArtifactSha256 {
        match self {
            Self::DeployBackend(s) => &s.artifact_sha256,
            Self::DeployClient(s) => &s.artifact_sha256,
            Self::DeployCli(s) => &s.artifact_sha256,
        }
    }

    pub fn manifest_sha256(&self) -> &ArtifactSha256 {
        match self {
            Self::DeployBackend(s) => &s.manifest_sha256,
            Self::DeployClient(s) => &s.manifest_sha256,
            Self::DeployCli(s) => &s.manifest_sha256,
        }
    }

    /// The artifact role this operation is allowed to deploy.
    pub fn expected_role(&self) -> ArtifactRole {
        match self {
            Self::DeployBackend(_) => ArtifactRole::Backend,
            Self::DeployClient(_) => ArtifactRole::Client,
            Self::DeployCli(_) => ArtifactRole::Cli,
        }
    }

    /// Self-contained checks: role match, canonical object key, and the
    /// role-specific parameters. Does not need (or grant) any authorisation.
    pub fn validate(&self) -> Result<(), String> {
        let id = self.artifact_identity();
        if id.role() != self.expected_role() {
            return Err(format!(
                "operation deploys the {:?} role but artifact_identity.role is {:?}",
                self.expected_role(),
                id.role()
            ));
        }
        let canonical = id
            .app_package_key()
            .map_err(|e| format!("artifact_identity has no package object key: {e}"))?;
        if self.package_object_key() != canonical {
            return Err(format!(
                "package_object_key {:?} is not the canonical key {canonical:?} of \
                 artifact_identity",
                self.package_object_key()
            ));
        }
        match self {
            Self::DeployBackend(s) => {
                if !s.preserve_config {
                    return Err("deploy_backend.preserve_config must be true".into());
                }
                if !s.require_db_backup {
                    return Err("deploy_backend.require_db_backup must be true".into());
                }
                if s.firewall == FirewallPolicy::ManagedPort(0) {
                    return Err("deploy_backend.firewall managed_port must be non-zero".into());
                }
            }
            Self::DeployClient(s) => {
                if !s.restart_gui_session_only {
                    return Err("deploy_client.restart_gui_session_only must be true".into());
                }
            }
            Self::DeployCli(s) => {
                if s.adopt_existing_cli && s.artifact_identity.os() != TargetOs::Macos {
                    return Err(
                        "deploy_cli.adopt_existing_cli is only valid for a macOS artifact".into(),
                    );
                }
            }
        }
        Ok(())
    }

    /// Exact comparison with the externally expected artifact.
    pub fn verify_binding(&self, expected: &ExpectedArtifact<'_>) -> Result<(), String> {
        if self.artifact_identity() != expected.identity {
            return Err("artifact_identity does not match the expected artifact identity".into());
        }
        if *self.artifact_sha256() != expected.integrity.sha256 {
            return Err("artifact_sha256 does not match the expected artifact integrity".into());
        }
        if self.manifest_sha256() != expected.manifest_sha256 {
            return Err("manifest_sha256 does not match the expected signed manifest".into());
        }
        Ok(())
    }
}

/// Plan reference stored on a typed job definition (`deploy_authorization`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeployAuthorizationRef {
    pub plan_id: String,
    pub plan_digest: ArtifactSha256,
}

impl schemars::JsonSchema for DeployAuthorizationRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DeployAuthorizationRef".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "required": ["plan_id", "plan_digest"],
            "properties": {
                "plan_id": {"type": "string", "pattern": "^[A-Za-z0-9._-]{1,128}$"},
                "plan_digest": {"type": "string", "pattern": "^[0-9a-f]{64}$"}
            },
            "additionalProperties": false
        })
    }
}

impl DeployAuthorizationRef {
    pub fn validate(&self) -> Result<(), String> {
        validate_plan_id(&self.plan_id)
    }
}

/// What a typed Command carries: the plan reference plus the digest of the
/// exact job manifest it was converted from.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeployAuthorization {
    pub plan_id: String,
    pub plan_digest: ArtifactSha256,
    pub manifest_digest: ArtifactSha256,
}

impl DeployAuthorization {
    pub fn validate(&self) -> Result<(), String> {
        validate_plan_id(&self.plan_id)
    }
}

fn validate_plan_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > MAX_PLAN_ID_LEN
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "deploy_authorization.plan_id must be 1..={MAX_PLAN_ID_LEN} characters of \
             [A-Za-z0-9._-] (got {id:?})"
        ));
    }
    Ok(())
}

/// Parse and bound a typed timeout / deadline-relevant duration.
pub fn parse_typed_timeout(raw: &str) -> Result<Duration, String> {
    let d = humantime::parse_duration(raw)
        .map_err(|e| format!("execute.timeout {raw:?} is not a duration: {e}"))?;
    if d < Duration::from_secs(1) || d > MAX_TYPED_TIMEOUT {
        return Err(format!(
            "execute.timeout {raw:?} must be between 1s and {}s for a typed operation",
            MAX_TYPED_TIMEOUT.as_secs()
        ));
    }
    Ok(d)
}

/// The typed branch of `Manifest.execute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedExecute {
    pub payload_version: u32,
    pub timeout: String,
    pub run_as: RunAs,
    pub operation: TypedOperation,
}

impl TypedExecute {
    pub fn validate(&self) -> Result<(), String> {
        if self.payload_version != TYPED_PAYLOAD_VERSION {
            return Err(format!(
                "unsupported typed payload_version {} (this build supports {TYPED_PAYLOAD_VERSION})",
                self.payload_version
            ));
        }
        if self.run_as != RunAs::System {
            return Err("a typed operation runs as `system` only".into());
        }
        parse_typed_timeout(&self.timeout)?;
        self.operation.validate()
    }
}

impl Serialize for TypedExecute {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("TypedExecute", 5)?;
        st.serialize_field("kind", KIND_TYPED_OPERATION)?;
        st.serialize_field("payload_version", &self.payload_version)?;
        st.serialize_field("timeout", &self.timeout)?;
        st.serialize_field("run_as", &self.run_as)?;
        st.serialize_field("operation", &self.operation)?;
        st.end()
    }
}

/// `Manifest.execute`: the historical shell form or a typed operation.
#[derive(Debug, Clone)]
pub enum ManifestExecute {
    Shell(Execute),
    Typed(TypedExecute),
}

impl From<Execute> for ManifestExecute {
    fn from(e: Execute) -> Self {
        Self::Shell(e)
    }
}

impl ManifestExecute {
    pub fn is_typed(&self) -> bool {
        matches!(self, Self::Typed(_))
    }

    pub fn shell(&self) -> Option<&Execute> {
        match self {
            Self::Shell(e) => Some(e),
            Self::Typed(_) => None,
        }
    }

    pub fn shell_mut(&mut self) -> Option<&mut Execute> {
        match self {
            Self::Shell(e) => Some(e),
            Self::Typed(_) => None,
        }
    }

    pub fn typed(&self) -> Option<&TypedExecute> {
        match self {
            Self::Typed(t) => Some(t),
            Self::Shell(_) => None,
        }
    }

    /// The only way for a shell-only consumer (script resolver, shell Command
    /// builder, local runner) to obtain an [`Execute`]. A typed job is refused,
    /// never reinterpreted.
    pub fn require_shell(&self) -> Result<&Execute, TypedDeployDisabled> {
        self.shell().ok_or(TypedDeployDisabled)
    }

    pub fn timeout(&self) -> &str {
        match self {
            Self::Shell(e) => &e.timeout,
            Self::Typed(t) => &t.timeout,
        }
    }

    pub fn run_as(&self) -> RunAs {
        match self {
            Self::Shell(e) => e.run_as,
            Self::Typed(t) => t.run_as,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Shell(e) => e.validate_script_source(),
            Self::Typed(t) => t.validate(),
        }
    }
}

impl Serialize for ManifestExecute {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            // No `kind` key: stored shell records stay byte-identical.
            Self::Shell(e) => e.serialize(s),
            Self::Typed(t) => t.serialize(s),
        }
    }
}

impl schemars::JsonSchema for ManifestExecute {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ManifestExecute".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let shell = generator.subschema_for::<Execute>();
        schemars::json_schema!({
            "description": "Either the shell form (no `kind`) or `kind: typed_operation`.",
            "anyOf": [
                shell,
                {
                    "type": "object",
                    "description": "Typed native operation. Delivery is disabled until the \
                                    shared approval/capability gates exist; no shell, script or \
                                    cwd fields are allowed.",
                    "required": ["kind", "payload_version", "timeout", "operation"],
                    "properties": {
                        "kind": {"const": KIND_TYPED_OPERATION},
                        "payload_version": {"const": TYPED_PAYLOAD_VERSION},
                        "timeout": {"type": "string"},
                        "run_as": {"const": "system"},
                        "operation": {
                            "type": "object",
                            "required": ["type", "spec"],
                            "properties": {
                                "type": {"enum": ["deploy_backend", "deploy_client", "deploy_cli"]},
                                "spec": {"type": "object"}
                            },
                            "additionalProperties": false
                        }
                    },
                    "additionalProperties": false
                }
            ]
        })
    }
}

/// Present-or-absent bookkeeping so `null` is never confused with a missing key.
struct Raw {
    kind: Option<String>,
    bypass_local_limit: Option<bool>,
    shell: Option<ExecuteShell>,
    script: Option<Option<String>>,
    script_file: Option<Option<String>>,
    script_object: Option<Option<String>>,
    timeout: Option<String>,
    run_as: Option<RunAs>,
    cwd: Option<Option<String>>,
    payload_version: Option<u32>,
    operation: Option<TypedOperation>,
    unknown: Vec<String>,
}

struct RawVisitor;

fn once<T, E: de::Error>(slot: &mut Option<T>, v: T, name: &'static str) -> Result<(), E> {
    if slot.replace(v).is_some() {
        return Err(E::duplicate_field(name));
    }
    Ok(())
}

impl<'de> Visitor<'de> for RawVisitor {
    type Value = Raw;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an execute mapping")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
        let mut raw = Raw {
            kind: None,
            bypass_local_limit: None,
            shell: None,
            script: None,
            script_file: None,
            script_object: None,
            timeout: None,
            run_as: None,
            cwd: None,
            payload_version: None,
            operation: None,
            unknown: Vec::new(),
        };
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "kind" => once::<_, A::Error>(&mut raw.kind, map.next_value()?, "kind")?,
                "bypass_local_limit" => once::<_, A::Error>(
                    &mut raw.bypass_local_limit,
                    map.next_value()?,
                    "bypass_local_limit",
                )?,
                "shell" => once::<_, A::Error>(&mut raw.shell, map.next_value()?, "shell")?,
                "script" => once::<_, A::Error>(&mut raw.script, map.next_value()?, "script")?,
                "script_file" => {
                    once::<_, A::Error>(&mut raw.script_file, map.next_value()?, "script_file")?
                }
                "script_object" => {
                    once::<_, A::Error>(&mut raw.script_object, map.next_value()?, "script_object")?
                }
                "timeout" => once::<_, A::Error>(&mut raw.timeout, map.next_value()?, "timeout")?,
                "run_as" => once::<_, A::Error>(&mut raw.run_as, map.next_value()?, "run_as")?,
                "cwd" => once::<_, A::Error>(&mut raw.cwd, map.next_value()?, "cwd")?,
                "payload_version" => once::<_, A::Error>(
                    &mut raw.payload_version,
                    map.next_value()?,
                    "payload_version",
                )?,
                "operation" => {
                    once::<_, A::Error>(&mut raw.operation, map.next_value()?, "operation")?
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    raw.unknown.push(key);
                }
            }
        }
        Ok(raw)
    }
}

impl<'de> Deserialize<'de> for ManifestExecute {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = d.deserialize_map(RawVisitor)?;
        raw.into_execute().map_err(de::Error::custom)
    }
}

impl Raw {
    fn shell_fields_present(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.bypass_local_limit.is_some() {
            out.push("bypass_local_limit");
        }
        if self.shell.is_some() {
            out.push("shell");
        }
        if self.script.is_some() {
            out.push("script");
        }
        if self.script_file.is_some() {
            out.push("script_file");
        }
        if self.script_object.is_some() {
            out.push("script_object");
        }
        if self.cwd.is_some() {
            out.push("cwd");
        }
        out
    }

    fn into_execute(self) -> Result<ManifestExecute, String> {
        match self.kind.as_deref() {
            None | Some(KIND_SHELL) => {
                // The discriminator may be omitted only for a record that has
                // no typed field at all: anything else is a malformed typed
                // request, which must not be read as a shell job.
                let typed: Vec<&str> = [
                    ("payload_version", self.payload_version.is_some()),
                    ("operation", self.operation.is_some()),
                ]
                .into_iter()
                .filter_map(|(n, p)| p.then_some(n))
                .collect();
                if !typed.is_empty() {
                    return Err(format!(
                        "execute carries typed field(s) {} without `kind: {KIND_TYPED_OPERATION}`; \
                         refusing to read it as a shell job",
                        typed.join(", ")
                    ));
                }
                Ok(ManifestExecute::Shell(Execute {
                    bypass_local_limit: self.bypass_local_limit.unwrap_or(false),
                    shell: self
                        .shell
                        .ok_or_else(|| "missing field `shell`".to_string())?,
                    script: self.script.unwrap_or(None),
                    script_file: self.script_file.unwrap_or(None),
                    script_object: self.script_object.unwrap_or(None),
                    timeout: self
                        .timeout
                        .ok_or_else(|| "missing field `timeout`".to_string())?,
                    run_as: self.run_as.unwrap_or_default(),
                    cwd: self.cwd.unwrap_or(None),
                }))
            }
            Some(KIND_TYPED_OPERATION) => {
                let mixed = self.shell_fields_present();
                if !mixed.is_empty() {
                    return Err(format!(
                        "a typed_operation execute must not carry shell field(s): {}",
                        mixed.join(", ")
                    ));
                }
                if !self.unknown.is_empty() {
                    return Err(format!(
                        "a typed_operation execute has unknown field(s): {}",
                        self.unknown.join(", ")
                    ));
                }
                let typed = TypedExecute {
                    payload_version: self
                        .payload_version
                        .ok_or_else(|| "missing field `payload_version`".to_string())?,
                    timeout: self
                        .timeout
                        .ok_or_else(|| "missing field `timeout`".to_string())?,
                    run_as: self.run_as.unwrap_or_default(),
                    operation: self
                        .operation
                        .ok_or_else(|| "missing field `operation`".to_string())?,
                };
                // Version is checked at parse time so an unsupported payload
                // never becomes a value other code could act on.
                if typed.payload_version != TYPED_PAYLOAD_VERSION {
                    return Err(format!(
                        "unsupported typed payload_version {} (this build supports \
                         {TYPED_PAYLOAD_VERSION})",
                        typed.payload_version
                    ));
                }
                Ok(ManifestExecute::Typed(typed))
            }
            Some(other) => Err(format!(
                "unknown execute kind {other:?}; expected `{KIND_TYPED_OPERATION}` or omit `kind` \
                 for a shell job"
            )),
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use crate::artifact::{ArtifactIdentity, ArtifactIntegrity, ArtifactSha256};
    use crate::manifest::Manifest;

    pub const ARTIFACT_BYTES: &[u8] = b"backend package bytes";
    pub const SIGNED_MANIFEST_BYTES: &[u8] = b"signed package manifest bytes";

    pub fn identity() -> ArtifactIdentity {
        serde_json::from_str(
            r#"{"role":"backend","version":"0.80.0","os":"linux","arch":"aarch64","distro_abi":"musl","format":"tar_gz"}"#,
        )
        .unwrap()
    }

    pub fn integrity() -> ArtifactIntegrity {
        ArtifactIntegrity::of(ARTIFACT_BYTES)
    }

    pub fn manifest_sha() -> ArtifactSha256 {
        ArtifactSha256::of(SIGNED_MANIFEST_BYTES)
    }

    pub fn plan_digest() -> ArtifactSha256 {
        ArtifactSha256::of(b"plan")
    }

    /// A complete, valid typed backend job as YAML.
    pub fn backend_yaml() -> String {
        format!(
            r#"id: deploy-backend
version: 1.0.0
require_approval: true
deploy_authorization:
  plan_id: plan-1
  plan_digest: {plan}
execute:
  kind: typed_operation
  payload_version: 1
  timeout: 10m
  run_as: system
  operation:
    type: deploy_backend
    spec:
      artifact_identity: {{role: backend, version: 0.80.0, os: linux, arch: aarch64, distro_abi: musl, format: tar_gz}}
      package_object_key: backend/0.80.0/linux-aarch64-musl.tar_gz
      artifact_sha256: {artifact}
      manifest_sha256: {manifest}
      preserve_config: true
      require_db_backup: true
      wipe_db: false
      start_policy: preserve
      firewall: preserve
"#,
            plan = hex(&plan_digest()),
            artifact = hex(&integrity().sha256),
            manifest = hex(&manifest_sha()),
        )
    }

    pub fn hex(d: &ArtifactSha256) -> String {
        String::from(*d)
    }

    pub fn backend_manifest() -> Manifest {
        crate::strict::from_yaml_str(&backend_yaml()).expect("valid typed job")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::manifest::Manifest;

    const SHELL_YAML: &str =
        "id: j\nversion: 1.0.0\nexecute:\n  shell: powershell\n  script: echo hi\n  timeout: 30s\n";

    fn parse(yaml: &str) -> Result<Manifest, String> {
        crate::strict::from_yaml_str(yaml)
    }

    fn typed_with(replace: &str, with: &str) -> String {
        let y = backend_yaml();
        assert!(y.contains(replace), "fixture lacks {replace:?}");
        y.replacen(replace, with, 1)
    }

    #[test]
    fn historical_shell_job_parses_and_round_trips_without_a_kind_key() {
        let m = parse(SHELL_YAML).unwrap();
        m.validate().unwrap();
        let e = m.execute.shell().expect("shell");
        assert_eq!(e.script.as_deref(), Some("echo hi"));
        let json = serde_json::to_value(&m).unwrap();
        // The stored record gains no discriminator and no typed field.
        assert!(json["execute"].get("kind").is_none());
        assert!(json["execute"].get("payload_version").is_none());
        assert!(json.get("deploy_authorization").is_none());
        let back: Manifest = serde_json::from_value(json).unwrap();
        assert!(!back.execute.is_typed());
    }

    #[test]
    fn historical_json_record_with_unknown_future_field_still_parses() {
        // Read paths stay tolerant for shell jobs (the fleet's gradual-upgrade rule).
        let m: Manifest = serde_json::from_str(
            r#"{"id":"j","version":"1","execute":{"shell":"sh","script":"x","timeout":"5s","future_field":1}}"#,
        )
        .unwrap();
        assert!(!m.execute.is_typed());
    }

    #[test]
    fn explicit_shell_kind_is_the_same_shell_job() {
        let y = SHELL_YAML.replace("execute:\n", "execute:\n  kind: shell\n");
        assert!(!parse(&y).unwrap().execute.is_typed());
    }

    #[test]
    fn strict_boundary_still_names_shell_typos() {
        let y = SHELL_YAML.replace("script:", "scriptt:");
        let err = parse(&y.replace("scriptt: echo hi", "script: echo hi\n  script_objectt: a/b"))
            .unwrap_err();
        assert!(err.contains("execute.script_objectt"), "{err}");
    }

    #[test]
    fn valid_typed_definition_parses_validates_and_round_trips() {
        let m = backend_manifest();
        m.validate().unwrap();
        let t = m.execute.typed().expect("typed");
        assert_eq!(t.payload_version, 1);
        assert_eq!(t.run_as, RunAs::System);
        assert!(matches!(t.operation, TypedOperation::DeployBackend(_)));
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        back.validate().unwrap();
        assert_eq!(back.execute.typed(), m.execute.typed());
        // The typed record carries no shell-side key at all.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for k in ["shell", "script", "script_file", "script_object", "cwd"] {
            assert!(
                v["execute"].get(k).is_none(),
                "{k} leaked into typed record"
            );
        }
        assert_eq!(v["execute"]["kind"], "typed_operation");
    }

    #[test]
    fn client_and_cli_definitions_validate() {
        // A client operation fed a backend spec is rejected, not coerced.
        let client = typed_with("type: deploy_backend", "type: deploy_client");
        assert!(parse(&client).is_err());

        let m = crate::strict::from_yaml_str::<Manifest>(&format!(
            r#"id: c
version: 1.0.0
require_approval: true
deploy_authorization: {{plan_id: p, plan_digest: {plan}}}
execute:
  kind: typed_operation
  payload_version: 1
  timeout: 5m
  operation:
    type: deploy_client
    spec:
      artifact_identity: {{role: client, version: 0.80.0, os: macos, arch: aarch64, distro_abi: darwin14, format: pkg}}
      package_object_key: client/0.80.0/macos-aarch64-darwin14.pkg
      artifact_sha256: {a}
      manifest_sha256: {m}
      opt_out_policy: respect
      restart_gui_session_only: true
"#,
            plan = hex(&plan_digest()),
            a = hex(&integrity().sha256),
            m = hex(&manifest_sha()),
        ))
        .unwrap();
        m.validate().unwrap();
        let cli = format!(
            r#"id: x
version: 1.0.0
require_approval: true
deploy_authorization: {{plan_id: p, plan_digest: {plan}}}
execute:
  kind: typed_operation
  payload_version: 1
  timeout: 5m
  operation:
    type: deploy_cli
    spec:
      artifact_identity: {{role: cli, version: 0.80.0, os: linux, arch: x86_64, distro_abi: musl, format: tar_gz}}
      package_object_key: cli/0.80.0/linux-x86_64-musl.tar_gz
      artifact_sha256: {a}
      manifest_sha256: {m}
      adopt_existing_cli: true
"#,
            plan = hex(&plan_digest()),
            a = hex(&integrity().sha256),
            m = hex(&manifest_sha()),
        );
        // adoption is a macOS-only, fixed-location concept.
        assert!(
            parse(&cli)
                .unwrap()
                .validate()
                .unwrap_err()
                .contains("macOS")
        );
        let ok = cli.replace("adopt_existing_cli: true", "adopt_existing_cli: false");
        parse(&ok).unwrap().validate().unwrap();
    }

    #[test]
    fn discriminator_omission_never_reads_typed_fields_as_shell() {
        // No kind, but typed-only fields: refused, not treated as shell.
        let y = typed_with("kind: typed_operation\n", "");
        let err = parse(&y).unwrap_err();
        assert!(err.contains("without `kind"), "{err}");
        let y = "id: j\nversion: 1\nexecute:\n  shell: sh\n  script: x\n  timeout: 5s\n  operation: {}\n";
        assert!(parse(y).is_err());
        let y = "id: j\nversion: 1\nexecute:\n  shell: sh\n  script: x\n  timeout: 5s\n  payload_version: 1\n";
        assert!(parse(y).is_err());
        // No kind and nothing else shell-like: missing `shell`, never a default shell.
        let y = typed_with("kind: typed_operation\n", "");
        assert!(parse(&y).is_err());
    }

    #[test]
    fn unknown_null_and_mistyped_discriminators_are_rejected() {
        for kind in [
            "deploy",
            "Typed_Operation",
            "null",
            "~",
            "1",
            "[typed_operation]",
            "''",
        ] {
            let y = typed_with("kind: typed_operation", &format!("kind: {kind}"));
            assert!(parse(&y).is_err(), "kind {kind} must be rejected");
        }
        // Same through JSON.
        for kind in ["null", "\"deploy\"", "5"] {
            let j = format!(
                r#"{{"id":"j","version":"1","execute":{{"kind":{kind},"shell":"sh","script":"x","timeout":"5s"}}}}"#
            );
            assert!(
                serde_json::from_str::<Manifest>(&j).is_err(),
                "json kind {kind}"
            );
        }
    }

    #[test]
    fn mixed_shell_and_typed_fields_are_rejected_by_name() {
        for extra in [
            "shell: sh",
            "script: echo hi",
            "script_file: a.sh",
            "script_object: a/b",
            "cwd: /tmp",
            "bypass_local_limit: true",
        ] {
            let y = typed_with("  timeout: 10m\n", &format!("  timeout: 10m\n  {extra}\n"));
            let err = parse(&y).unwrap_err();
            assert!(err.contains("must not carry shell field"), "{extra}: {err}");
        }
        // `cwd: null` is still present, never silently absent.
        let y = typed_with("  timeout: 10m\n", "  timeout: 10m\n  cwd: null\n");
        assert!(parse(&y).is_err());
    }

    #[test]
    fn unknown_fields_and_operations_in_a_typed_record_are_rejected_on_every_read() {
        let y = typed_with("  timeout: 10m\n", "  timeout: 10m\n  surprise: 1\n");
        assert!(parse(&y).unwrap_err().contains("surprise"));
        // Plain (tolerant) serde read rejects it too.
        let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
        assert!(serde_yaml::from_value::<Manifest>(v).is_err());
        let y = typed_with("type: deploy_backend", "type: deploy_everything");
        assert!(parse(&y).is_err());
        let y = typed_with(
            "      wipe_db: false\n",
            "      wipe_db: false\n      install_path: /tmp/x\n",
        );
        assert!(
            parse(&y).is_err(),
            "unknown spec field (arbitrary path) must be rejected"
        );
        let y = typed_with(
            "      firewall: preserve\n",
            "      firewall: open_everything\n",
        );
        assert!(parse(&y).is_err());
        let y = typed_with("    spec:\n", "    extra: 1\n    spec:\n");
        assert!(parse(&y).is_err(), "unknown operation key");
    }

    #[test]
    fn duplicate_keys_are_rejected_in_json_and_yaml() {
        let j = backend_manifest();
        let json = serde_json::to_string(&j).unwrap();
        let dup = json.replacen(
            "\"payload_version\":1,",
            "\"payload_version\":1,\"payload_version\":2,",
            1,
        );
        assert!(serde_json::from_str::<Manifest>(&dup).is_err());
        let dup = json.replacen(
            "\"wipe_db\":false,",
            "\"wipe_db\":false,\"wipe_db\":true,",
            1,
        );
        assert!(serde_json::from_str::<Manifest>(&dup).is_err());
        let dup = json.replacen(
            "\"kind\":\"typed_operation\",",
            "\"kind\":\"typed_operation\",\"kind\":\"shell\",",
            1,
        );
        assert!(serde_json::from_str::<Manifest>(&dup).is_err());
        let y = typed_with("  timeout: 10m\n", "  timeout: 10m\n  timeout: 20m\n");
        assert!(parse(&y).is_err());
    }

    #[test]
    fn unsupported_or_missing_payload_versions_are_rejected() {
        for v in ["0", "2", "99"] {
            let y = typed_with("payload_version: 1", &format!("payload_version: {v}"));
            assert!(
                parse(&y)
                    .unwrap_err()
                    .contains("unsupported typed payload_version")
            );
        }
        let y = typed_with("  payload_version: 1\n", "");
        assert!(parse(&y).unwrap_err().contains("payload_version"));
        let y = typed_with("payload_version: 1", "payload_version: null");
        assert!(parse(&y).is_err());
    }

    #[test]
    fn identity_must_be_canonical_and_match_the_role() {
        // Malformed identity (unknown OS token, alias version).
        for (from, to) in [
            ("os: linux", "os: LINUX"),
            ("version: 0.80.0, os", "version: v0.80.0, os"),
            ("format: tar_gz}", "format: zip}"),
            ("role: backend,", "role: bogus,"),
        ] {
            let y = typed_with(from, to);
            assert!(parse(&y).is_err(), "{from} -> {to}");
        }
        // Valid identity of another role: mismatch.
        let y = typed_with("role: backend, version", "role: cli, version");
        let m = parse(&y).unwrap();
        assert!(m.validate().unwrap_err().contains("role"));
        // Object key must be exactly the canonical key.
        for key in [
            "backend/0.80.0/linux-x86_64-musl.tar_gz",
            "backend/0.80.0/../0.80.0/linux-aarch64-musl.tar_gz",
            "Backend/0.80.0/linux-aarch64-musl.tar_gz",
            "linux-aarch64-musl.tar_gz",
        ] {
            let y = typed_with("backend/0.80.0/linux-aarch64-musl.tar_gz", key);
            assert!(
                parse(&y)
                    .unwrap()
                    .validate()
                    .unwrap_err()
                    .contains("canonical key"),
                "{key}"
            );
        }
        // Digests are strict lowercase hex.
        let y = typed_with(
            &hex(&integrity().sha256),
            &hex(&integrity().sha256).to_uppercase(),
        );
        assert!(parse(&y).is_err());
        let y = typed_with(&hex(&integrity().sha256), "abc");
        assert!(parse(&y).is_err());
    }

    #[test]
    fn role_specific_parameters_are_enforced() {
        for (from, to, needle) in [
            (
                "preserve_config: true",
                "preserve_config: false",
                "preserve_config",
            ),
            (
                "require_db_backup: true",
                "require_db_backup: false",
                "require_db_backup",
            ),
            (
                "firewall: preserve",
                "firewall: {managed_port: 0}",
                "managed_port",
            ),
        ] {
            let y = typed_with(from, to);
            let err = parse(&y).unwrap().validate().unwrap_err();
            assert!(err.contains(needle), "{err}");
        }
        let y = typed_with("firewall: preserve", "firewall: {managed_port: 8443}");
        parse(&y).unwrap().validate().unwrap();
        let y = typed_with("preserve_config: true\n", "");
        assert!(parse(&y).is_err(), "required spec field missing");
    }

    #[test]
    fn typed_jobs_run_as_system_with_bounded_timeouts_only() {
        let y = typed_with("run_as: system", "run_as: user");
        assert!(
            parse(&y)
                .unwrap()
                .validate()
                .unwrap_err()
                .contains("system")
        );
        let y = typed_with("run_as: system", "run_as: system_gui");
        assert!(parse(&y).unwrap().validate().is_err());
        for t in ["0s", "500ms", "2h", "forever"] {
            let y = typed_with("timeout: 10m", &format!("timeout: {t}"));
            assert!(parse(&y).unwrap().validate().is_err(), "timeout {t}");
        }
    }

    #[test]
    fn job_level_contract_requires_approval_and_plan_reference() {
        let y = typed_with("require_approval: true", "require_approval: false");
        assert!(
            parse(&y)
                .unwrap()
                .validate()
                .unwrap_err()
                .contains("require_approval")
        );
        let y = typed_with(
            &format!(
                "deploy_authorization:\n  plan_id: plan-1\n  plan_digest: {}\n",
                hex(&plan_digest())
            ),
            "",
        );
        assert!(
            parse(&y)
                .unwrap()
                .validate()
                .unwrap_err()
                .contains("deploy_authorization")
        );
        let y = typed_with("plan_id: plan-1", "plan_id: 'plan 1/../x'");
        assert!(parse(&y).unwrap().validate().is_err());
        let y = typed_with(
            "version: 1.0.0\n",
            "version: 1.0.0\ninventory:\n  display: {}\n",
        );
        assert!(parse(&y).is_err() || parse(&y).unwrap().validate().is_err());
        // A plan reference on a shell job is meaningless and refused.
        let y = format!(
            "{SHELL_YAML}deploy_authorization:\n  plan_id: p\n  plan_digest: {}\n",
            hex(&plan_digest())
        );
        assert!(
            parse(&y)
                .unwrap()
                .validate()
                .unwrap_err()
                .contains("only valid")
        );
    }

    #[test]
    fn typed_jobs_cannot_reach_the_shell_script_resolver() {
        let m = backend_manifest();
        // The only accessor the shell consumers use refuses a typed job.
        assert_eq!(m.execute.require_shell().unwrap_err(), TypedDeployDisabled);
        assert!(m.execute.shell().is_none());
        // And a shell job is still served.
        assert!(parse(SHELL_YAML).unwrap().execute.require_shell().is_ok());
        assert_eq!(m.execute.timeout(), "10m");
    }

    #[test]
    fn binding_requires_exact_identity_integrity_and_signed_manifest() {
        let m = backend_manifest();
        let op = &m.execute.typed().unwrap().operation;
        let id = identity();
        let integ = integrity();
        let msha = manifest_sha();
        let ok = ExpectedArtifact {
            identity: &id,
            integrity: &integ,
            manifest_sha256: &msha,
        };
        op.verify_binding(&ok).unwrap();

        let other_id: ArtifactIdentity = serde_json::from_str(
            r#"{"role":"backend","version":"0.81.0","os":"linux","arch":"aarch64","distro_abi":"musl","format":"tar_gz"}"#,
        )
        .unwrap();
        assert!(
            op.verify_binding(&ExpectedArtifact {
                identity: &other_id,
                ..ok
            })
            .is_err()
        );
        let other_int = ArtifactIntegrity::of(b"different");
        assert!(
            op.verify_binding(&ExpectedArtifact {
                integrity: &other_int,
                ..ok
            })
            .is_err()
        );
        let other_sha = ArtifactSha256::of(b"another manifest");
        assert!(
            op.verify_binding(&ExpectedArtifact {
                manifest_sha256: &other_sha,
                ..ok
            })
            .is_err()
        );
    }

    #[test]
    fn schema_documents_the_union_without_shell_in_the_typed_branch() {
        let schema = serde_json::to_value(schemars::schema_for!(Manifest)).unwrap();
        let text = schema.to_string();
        assert!(text.contains("typed_operation"));
        assert!(text.contains("deploy_authorization"));
    }
}
