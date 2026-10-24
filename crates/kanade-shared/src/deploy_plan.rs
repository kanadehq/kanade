//! Deployment plan contract and the shared authorization / preflight gate
//! (SPEC §2.13.5.6, §2.13.6.2–3; migration stage 4).
//!
//! Everything here is **pure**: no database, no clock, no network. The backend
//! persists [`PlanContent`] and feeds [`evaluate`] with the stored record, the
//! caller's current authorization and per-target capability evidence; the
//! agent-side boundaries (KLP, local scheduler) call [`local_refusal`]
//! because they hold no delegated backend authorization.
//!
//! Three separate decisions are never merged:
//!
//! * **publisher trust** — who produced the package (`publisher_trust`);
//! * **package admission** — this exact package passed trust verification
//!   (`package_admission`); and
//! * **operator deployment approval** — a human approved *this* plan for
//!   *these* targets (this module).
//!
//! A plan binds its approval to a deterministic [`PlanContent::digest`]; a plan
//! cannot be edited, only replaced by a new one. Passing every check in
//! [`evaluate`] still does not dispatch anything: [`dispatch`] ends in
//! [`DeployRefusal::HandlerUnavailable`] until real native handlers exist, and
//! there is no switch that skips it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::artifact::{
    ArtifactIdentity, ArtifactIntegrity, ArtifactRole, ArtifactSha256, TargetArch,
};
use crate::manifest::Manifest;
use crate::typed_operation::{
    DeployAuthorizationRef, ExpectedArtifact, MAX_PLAN_ID_LEN, TYPED_DEPLOY_DISABLED,
    TypedOperation,
};
use crate::wire::{DeployCapabilities, PROTOCOL_V2};

/// Version of the plan record / digest rules.
pub const PLAN_RECORD_VERSION: u32 = 1;
/// Domain prefix of the plan digest (ASCII, NUL terminated).
pub const PLAN_DIGEST_DOMAIN: &[u8] = b"kanade/deploy-plan/sha256/v1\0";
/// Domain prefix of the typed job manifest digest.
pub const JOB_DIGEST_DOMAIN: &[u8] = b"kanade/deploy-job-manifest/sha256/v1\0";
/// Longest span a plan may stay valid (`expires_at - not_before`, and from
/// registration). A proposal: SPEC leaves the cap to the operator.
pub const MAX_PLAN_VALIDITY_SECS: i64 = 24 * 60 * 60;
/// Most targets one plan may name.
pub const MAX_PLAN_TARGETS: usize = 1000;
/// Oldest capability evidence (backend receipt time) a preflight accepts. A
/// proposal: roughly ten heartbeat intervals at the default cadence.
pub const MAX_CAPABILITY_EVIDENCE_AGE_SECS: i64 = 300;

/// How many times a plan may be applied to each target.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPolicy {
    /// Each (plan, target) pair is consumed at most once. Anything else
    /// (repeat, schedule, waves) is not accepted until it is designed.
    SingleUsePerTarget,
}

/// The immutable artifact the plan was made for, as the operator expects it
/// (from the artifact store / signed package manifest), never read back from
/// the job.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanArtifact {
    pub identity: ArtifactIdentity,
    pub integrity: ArtifactIntegrity,
    /// SHA-256 of the exact signed `package.manifest.json` bytes.
    pub manifest_sha256: ArtifactSha256,
}

impl PlanArtifact {
    pub fn expected(&self) -> ExpectedArtifact<'_> {
        ExpectedArtifact {
            identity: &self.identity,
            integrity: &self.integrity,
            manifest_sha256: &self.manifest_sha256,
        }
    }
}

/// Everything a plan approves. Serialised canonically and hashed into
/// [`Self::digest`]; the approval is bound to that digest.
///
/// Targets are exact registered `pc_id`s: case is preserved and compared byte
/// for byte, and the list is digested in ascending byte order with no
/// duplicates.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanContent {
    pub record_version: u32,
    pub plan_id: String,
    pub job_id: String,
    /// Digest of the typed job manifest with `deploy_authorization` removed
    /// (which names this plan, so it cannot be part of its own input).
    pub job_manifest_digest: ArtifactSha256,
    pub role: ArtifactRole,
    /// The role-specific operation and its explicit options.
    pub operation: TypedOperation,
    pub artifact: PlanArtifact,
    pub targets: Vec<String>,
    /// Unix seconds, inclusive.
    pub not_before: i64,
    /// Unix seconds, exclusive.
    pub expires_at: i64,
    pub execution_policy: ExecutionPolicy,
}

/// JSON with object keys sorted bytewise and no whitespace: one byte string per
/// value, independent of struct field order or serde's map implementation.
pub fn canonical_json(v: &Value) -> Vec<u8> {
    fn go(v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push(b'{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend(serde_json::to_vec(k).expect("string serializes"));
                    out.push(b':');
                    go(&m[k], out);
                }
                out.push(b'}');
            }
            Value::Array(a) => {
                out.push(b'[');
                for (i, e) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    go(e, out);
                }
                out.push(b']');
            }
            other => out.extend(serde_json::to_vec(other).expect("scalar serializes")),
        }
    }
    let mut out = Vec::new();
    go(v, &mut out);
    out
}

fn domain_sha(domain: &[u8], v: &Value) -> ArtifactSha256 {
    let mut h = Sha256::new();
    h.update(domain);
    h.update(canonical_json(v));
    ArtifactSha256::from_bytes(h.finalize().into())
}

/// Digest of a typed job manifest with its `deploy_authorization` removed.
/// Only a typed job has one; a shell job is an error.
pub fn job_manifest_digest(manifest: &Manifest) -> Result<ArtifactSha256, String> {
    if !manifest.execute.is_typed() {
        return Err("only a typed_operation job has a deployment digest".into());
    }
    let mut v = serde_json::to_value(manifest).map_err(|e| e.to_string())?;
    v.as_object_mut()
        .ok_or("job manifest is not an object")?
        .remove("deploy_authorization");
    Ok(domain_sha(JOB_DIGEST_DOMAIN, &v))
}

fn validate_plan_id(id: &str) -> Result<(), String> {
    DeployAuthorizationRef {
        plan_id: id.to_owned(),
        plan_digest: ArtifactSha256::from_bytes([0; 32]),
    }
    .validate()
}

impl PlanContent {
    /// Deterministic digest: SHA-256 over [`PLAN_DIGEST_DOMAIN`] and the
    /// canonical JSON of the whole record.
    pub fn digest(&self) -> ArtifactSha256 {
        domain_sha(
            PLAN_DIGEST_DOMAIN,
            &serde_json::to_value(self).expect("plan content serializes"),
        )
    }

    /// Structural validity, independent of the clock and of any job.
    pub fn validate(&self) -> Result<(), String> {
        if self.record_version != PLAN_RECORD_VERSION {
            return Err(format!(
                "unsupported plan record_version {} (this build supports {PLAN_RECORD_VERSION})",
                self.record_version
            ));
        }
        validate_plan_id(&self.plan_id)?;
        if self.job_id.is_empty() || self.job_id.len() > MAX_PLAN_ID_LEN {
            return Err("job_id must be 1..=128 characters".into());
        }
        if self.role != self.operation.expected_role() {
            return Err(format!(
                "plan role {:?} does not match the operation's role {:?}; agent self-update \
                 keeps using the existing publish / rollout path and has no deployment plan",
                self.role,
                self.operation.expected_role()
            ));
        }
        self.operation.validate()?;
        self.operation.verify_binding(&self.artifact.expected())?;
        if self.targets.is_empty() || self.targets.len() > MAX_PLAN_TARGETS {
            return Err(format!("a plan names 1..={MAX_PLAN_TARGETS} targets"));
        }
        for pair in self.targets.windows(2) {
            if pair[0] >= pair[1] {
                return Err("plan targets must be unique and in ascending byte order".into());
            }
        }
        for t in &self.targets {
            if t.is_empty() {
                return Err("an empty target pc_id is not allowed".into());
            }
            crate::subject::validate_pc_id(t)?;
        }
        if self.expires_at <= self.not_before {
            return Err("expires_at must be after not_before".into());
        }
        if self.expires_at - self.not_before > MAX_PLAN_VALIDITY_SECS {
            return Err(format!(
                "a plan may be valid for at most {MAX_PLAN_VALIDITY_SECS}s"
            ));
        }
        Ok(())
    }

    /// Sort `targets` (rejecting duplicates) into digest order.
    pub fn normalize_targets(targets: &[String]) -> Result<Vec<String>, String> {
        let mut t = targets.to_vec();
        t.sort();
        if t.windows(2).any(|p| p[0] == p[1]) {
            return Err("duplicate target pc_id in the plan".into());
        }
        Ok(t)
    }

    /// Check a job manifest against this plan: typed and valid, naming exactly
    /// this plan id and digest, with the digest of the approved job and the
    /// operation / artifact the plan binds.
    pub fn verify_job(&self, manifest: &Manifest) -> Result<(), String> {
        let typed = manifest
            .execute
            .typed()
            .ok_or("the job is not a typed_operation job")?;
        manifest.validate()?;
        if manifest.id != self.job_id {
            return Err(format!(
                "job id {:?} differs from the plan's job {:?}",
                manifest.id, self.job_id
            ));
        }
        let reference = manifest
            .deploy_authorization
            .as_ref()
            .ok_or("the job has no deploy_authorization")?;
        if reference.plan_id != self.plan_id {
            return Err("the job names a different plan".into());
        }
        if reference.plan_digest != self.digest() {
            return Err("the job's plan_digest differs from the plan's digest".into());
        }
        if job_manifest_digest(manifest)? != self.job_manifest_digest {
            return Err("the job manifest differs from the one the plan approved".into());
        }
        if typed.operation != self.operation {
            return Err("the job operation differs from the plan's operation".into());
        }
        typed.operation.verify_binding(&self.artifact.expected())
    }
}

/// Lifecycle of a stored plan. "Expired" is not a state: it is `now` outside
/// the validity window, evaluated against every use.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Pending,
    Approved,
    Revoked,
}

/// A stored plan as the gate sees it.
#[derive(Debug, Clone)]
pub struct PlanRecord {
    pub content: PlanContent,
    /// The digest as stored; the gate recomputes and compares it.
    pub stored_digest: ArtifactSha256,
    pub state: PlanState,
    pub approved_by: Option<String>,
    pub revoked_by: Option<String>,
    /// Targets already consumed under the single-use policy.
    pub applied_targets: Vec<String>,
}

/// The caller's *current* authorization, derived from the authenticated
/// request context — never from request fields.
#[derive(Debug, Clone)]
pub struct Principal {
    pub subject: String,
    /// Role is at least `operator`.
    pub operator: bool,
    /// The account may reach the Exec feature (page allow-list).
    pub exec_feature: bool,
}

/// What entered the dispatch boundary.
#[derive(Debug, Clone)]
pub enum DispatchActor {
    /// An authenticated operator request (`POST /api/exec/{job_id}`).
    Operator(Principal),
    /// The backend scheduler: it has no approval of its own and consuming a
    /// plan from a schedule is not designed yet.
    Scheduler,
    /// An agent-local path that holds no delegated backend authorization.
    Local(Boundary),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Klp,
    AgentScheduler,
    LocalDispatch,
    Cli,
}

impl Boundary {
    pub fn as_str(self) -> &'static str {
        match self {
            Boundary::Klp => "KLP",
            Boundary::AgentScheduler => "agent local scheduler",
            Boundary::LocalDispatch => "local dispatch",
            Boundary::Cli => "CLI",
        }
    }
}

/// The scope an exec request asked for, before any resolution.
#[derive(Debug, Clone, Default)]
pub struct RequestedScope {
    pub pcs: Vec<String>,
    pub all: bool,
    pub groups: bool,
    pub rollout: bool,
}

/// Capability evidence the backend holds for one target. Every field comes
/// from the same heartbeat; absence stays absence.
#[derive(Debug, Clone, Default)]
pub struct CapabilityEvidence {
    pub capabilities: Option<DeployCapabilities>,
    pub agent_version: Option<String>,
    pub protocols: Option<Vec<String>>,
    pub keys: Option<Vec<String>>,
    pub os_family: Option<String>,
    /// Backend receipt time (unix seconds) of that heartbeat.
    pub received_at: Option<i64>,
}

/// What the backend knows about one requested target right now.
#[derive(Debug, Clone)]
pub struct TargetFacts {
    pub pc_id: String,
    /// `agents` row exists with exactly this (case-sensitive) pc_id.
    pub registered: bool,
    pub current_agent_version: Option<String>,
    pub current_os_family: Option<String>,
    pub evidence: CapabilityEvidence,
}

/// Why one target failed preflight.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreflightFailure {
    #[error("not a registered agent (pc_id match is case-sensitive)")]
    NotRegistered,
    #[error("no deploy capability report received (older agent or never reported)")]
    CapabilitiesUnknown,
    #[error("reports no typed deployment handler")]
    CapabilitiesEmpty,
    #[error("capability evidence is stale or has no receipt time")]
    EvidenceStale,
    #[error("agent version changed since the capability report; wait for a fresh heartbeat")]
    AgentVersionChanged,
    #[error("os does not match the artifact (target reports {reported:?}, artifact is {artifact})")]
    OsMismatch {
        reported: Option<String>,
        artifact: &'static str,
    },
    #[error("no reported handler for {operation} on {os}/{arch}")]
    NoMatchingHandler {
        operation: &'static str,
        os: &'static str,
        arch: &'static str,
    },
    #[error("command protocols not reported")]
    ProtocolsUnknown,
    #[error("agent does not verify kanade.command.v2")]
    ProtocolMissing,
    #[error("the backend has no command-signing key, so no envelope can be signed")]
    SignerUnavailable,
    #[error("command keys not reported")]
    KeysUnknown,
    #[error("agent does not trust the backend's current signing key (kid and fingerprint)")]
    KeyMismatch,
}

/// Every reason the shared gate refuses. Each is a hard stop.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeployRefusal {
    #[error("typed deployment refused: {0}")]
    InvalidJob(String),
    #[error("typed deployment refused: deploy plan {0:?} does not exist")]
    PlanNotFound(String),
    #[error("typed deployment refused: plan integrity check failed: {0}")]
    PlanIntegrity(String),
    #[error("typed deployment refused: the job does not match the approved plan: {0}")]
    JobMismatch(String),
    #[error("typed deployment refused: plan is not approved")]
    NotApproved,
    #[error("typed deployment refused: plan was revoked")]
    Revoked,
    #[error("typed deployment refused: plan is not valid yet")]
    NotYetValid,
    #[error("typed deployment refused: plan expired")]
    Expired,
    #[error("typed deployment refused: plan approval state is ambiguous: {0}")]
    AmbiguousState(String),
    #[error(
        "typed deployment refused: the caller is not currently authorized (operator role and \
         exec permission are required)"
    )]
    NotAuthorized,
    #[error(
        "typed deployment refused: {0} holds no delegated backend authorization for a typed \
         deployment"
    )]
    NoDelegatedAuthorization(&'static str),
    #[error("typed deployment refused: requested targets do not match the approved plan: {0}")]
    TargetMismatch(String),
    #[error("typed deployment refused: plan was already applied to {0:?}")]
    Replay(String),
    #[error("typed deployment refused: preflight failed for {pc_id:?}: {reason}")]
    Preflight {
        pc_id: String,
        reason: PreflightFailure,
    },
    #[error("{}", TYPED_DEPLOY_DISABLED)]
    HandlerUnavailable,
}

/// The refusal for a path with no delegated backend authorization (KLP, agent
/// scheduler, local dispatch, CLI). The local path must never build a typed
/// Command itself.
pub fn local_refusal(boundary: Boundary) -> DeployRefusal {
    DeployRefusal::NoDelegatedAuthorization(boundary.as_str())
}

/// Per-target preflight against current capability evidence and the backend's
/// command-signing identity (`kid:fingerprint`).
///
/// Fails closed on absent, empty, stale and unknown evidence. It is only for
/// typed deployment: agent self-update (publish / rollout) never calls it.
pub fn preflight_target(
    artifact: &ArtifactIdentity,
    operation: &'static str,
    facts: &TargetFacts,
    signer: Option<&str>,
    now: i64,
) -> Result<(), PreflightFailure> {
    use PreflightFailure as F;
    if !facts.registered {
        return Err(F::NotRegistered);
    }
    let ev = &facts.evidence;
    let caps = ev.capabilities.as_ref().ok_or(F::CapabilitiesUnknown)?;
    if caps.handlers.is_empty() {
        return Err(F::CapabilitiesEmpty);
    }
    match ev.received_at {
        Some(at) if at <= now && now - at <= MAX_CAPABILITY_EVIDENCE_AGE_SECS => {}
        _ => return Err(F::EvidenceStale),
    }
    // The version the agent runs now must be the one that made the report; a
    // later heartbeat of another version (even one that omitted capabilities)
    // invalidates the old evidence instead of refreshing it.
    match (&facts.current_agent_version, &ev.agent_version) {
        (Some(now_v), Some(then_v)) if now_v == then_v => {}
        _ => return Err(F::AgentVersionChanged),
    }
    let os = artifact.os();
    let os_ok = |s: &Option<String>| s.as_deref() == Some(os.as_str());
    if !os_ok(&facts.current_os_family) || !os_ok(&ev.os_family) {
        return Err(F::OsMismatch {
            reported: facts
                .current_os_family
                .clone()
                .or_else(|| ev.os_family.clone()),
            artifact: os.as_str(),
        });
    }
    let arch: TargetArch = artifact.arch();
    if !caps.supports(operation, os, arch) {
        return Err(F::NoMatchingHandler {
            operation,
            os: os.as_str(),
            arch: arch.as_str(),
        });
    }
    let protocols = ev.protocols.as_ref().ok_or(F::ProtocolsUnknown)?;
    if !protocols.iter().any(|p| p == PROTOCOL_V2) {
        return Err(F::ProtocolMissing);
    }
    let signer = signer.ok_or(F::SignerUnavailable)?;
    let keys = ev.keys.as_ref().ok_or(F::KeysUnknown)?;
    if !keys.iter().any(|k| k == signer) {
        return Err(F::KeyMismatch);
    }
    Ok(())
}

/// Inputs of [`evaluate`].
pub struct GateInput<'a> {
    pub manifest: &'a Manifest,
    /// The stored plan named by the job's `deploy_authorization`, if any.
    pub plan: Option<&'a PlanRecord>,
    pub actor: &'a DispatchActor,
    pub scope: &'a RequestedScope,
    /// Facts for every requested target, in any order.
    pub targets: &'a [TargetFacts],
    /// `kid:fingerprint` of the backend's current command-signing key.
    pub signer: Option<&'a str>,
    pub now: i64,
}

/// A request that passed every authorization and preflight check. It is
/// evidence for a future delivery path, not a delivery: see [`dispatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedDeployment {
    pub plan_id: String,
    pub plan_digest: ArtifactSha256,
    pub targets: Vec<String>,
}

/// The shared authorization and preflight gate. Pure and total: every refusal
/// is a [`DeployRefusal`], and `Ok` only means "would be allowed if a handler
/// existed".
pub fn evaluate(input: &GateInput<'_>) -> Result<AuthorizedDeployment, DeployRefusal> {
    let principal = match input.actor {
        DispatchActor::Operator(p) => p,
        DispatchActor::Scheduler => {
            return Err(DeployRefusal::NoDelegatedAuthorization("the scheduler"));
        }
        DispatchActor::Local(b) => return Err(local_refusal(*b)),
    };
    let typed =
        input.manifest.execute.typed().ok_or_else(|| {
            DeployRefusal::InvalidJob("the job is not a typed_operation job".into())
        })?;
    input
        .manifest
        .validate()
        .map_err(DeployRefusal::InvalidJob)?;
    let reference = input
        .manifest
        .deploy_authorization
        .as_ref()
        .ok_or_else(|| DeployRefusal::InvalidJob("missing deploy_authorization".into()))?;
    let plan = input
        .plan
        .ok_or_else(|| DeployRefusal::PlanNotFound(reference.plan_id.clone()))?;

    // Integrity: the stored record must still hash to its stored digest, be
    // structurally valid, and match what the job references.
    plan.content
        .validate()
        .map_err(DeployRefusal::PlanIntegrity)?;
    let digest = plan.content.digest();
    if digest != plan.stored_digest {
        return Err(DeployRefusal::PlanIntegrity(
            "stored digest does not match the plan content".into(),
        ));
    }
    if reference.plan_id != plan.content.plan_id {
        return Err(DeployRefusal::PlanIntegrity("plan id mismatch".into()));
    }
    plan.content
        .verify_job(input.manifest)
        .map_err(DeployRefusal::JobMismatch)?;

    // State, with the approval columns cross-checked: an approved plan without
    // an approver, or any plan carrying contradictory columns, is ambiguous.
    match plan.state {
        PlanState::Pending => {
            if plan.approved_by.is_some() || plan.revoked_by.is_some() {
                return Err(DeployRefusal::AmbiguousState(
                    "pending plan carries approval or revocation data".into(),
                ));
            }
            return Err(DeployRefusal::NotApproved);
        }
        PlanState::Revoked => return Err(DeployRefusal::Revoked),
        PlanState::Approved => {
            if plan.approved_by.is_none() || plan.revoked_by.is_some() {
                return Err(DeployRefusal::AmbiguousState(
                    "approved plan has no approver or also carries a revocation".into(),
                ));
            }
        }
    }
    if input.now < plan.content.not_before {
        return Err(DeployRefusal::NotYetValid);
    }
    if input.now >= plan.content.expires_at {
        return Err(DeployRefusal::Expired);
    }

    // Current authorization of the dispatching caller (the approval is not a
    // standing grant to everyone).
    if !(principal.operator && principal.exec_feature) || principal.subject.is_empty() {
        return Err(DeployRefusal::NotAuthorized);
    }

    // Exact target set: explicit pcs only, byte-equal to the plan.
    if input.scope.all || input.scope.groups || input.scope.rollout {
        return Err(DeployRefusal::TargetMismatch(
            "a typed deployment names its exact approved pc_ids; all / group / rollout scopes \
             are not accepted"
                .into(),
        ));
    }
    let requested =
        PlanContent::normalize_targets(&input.scope.pcs).map_err(DeployRefusal::TargetMismatch)?;
    if requested != plan.content.targets {
        return Err(DeployRefusal::TargetMismatch(
            "the requested pc_id set differs from the approved set (exact, case-sensitive)".into(),
        ));
    }

    // Single use per target.
    if let Some(done) = plan
        .content
        .targets
        .iter()
        .find(|t| plan.applied_targets.contains(t))
    {
        return Err(DeployRefusal::Replay(done.clone()));
    }

    // Per-target preflight; every target needs facts.
    let operation = operation_token(&typed.operation);
    for pc in &plan.content.targets {
        let facts = input
            .targets
            .iter()
            .find(|f| &f.pc_id == pc)
            .ok_or_else(|| DeployRefusal::Preflight {
                pc_id: pc.clone(),
                reason: PreflightFailure::NotRegistered,
            })?;
        preflight_target(
            &plan.content.artifact.identity,
            operation,
            facts,
            input.signer,
            input.now,
        )
        .map_err(|reason| DeployRefusal::Preflight {
            pc_id: pc.clone(),
            reason,
        })?;
    }

    Ok(AuthorizedDeployment {
        plan_id: plan.content.plan_id.clone(),
        plan_digest: digest,
        targets: plan.content.targets.clone(),
    })
}

fn operation_token(op: &TypedOperation) -> &'static str {
    match op {
        TypedOperation::DeployBackend(_) => crate::wire::OPERATION_DEPLOY_BACKEND,
        TypedOperation::DeployClient(_) => crate::wire::OPERATION_DEPLOY_CLIENT,
        TypedOperation::DeployCli(_) => crate::wire::OPERATION_DEPLOY_CLI,
    }
}

/// The only production entry: [`evaluate`], then the independent
/// handler-availability barrier. No native handler exists, so this never
/// returns `Ok`; the barrier is not configurable and is not bypassed by a
/// plan, an approval or any capability evidence.
pub fn dispatch(input: &GateInput<'_>) -> Result<AuthorizedDeployment, DeployRefusal> {
    let authorized = evaluate(input)?;
    handler_barrier()?;
    Ok(authorized)
}

/// Native handlers are not implemented (SPEC §2.13 Task 5).
fn handler_barrier() -> Result<(), DeployRefusal> {
    Err(DeployRefusal::HandlerUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_operation::fixtures::*;
    use crate::wire::{DeployHandler, OPERATION_DEPLOY_BACKEND, TYPED_DEPLOY_CONTRACT_V1};

    const NOW: i64 = 1_800_000_000;
    const SIGNER: &str = "backend-1:abcd";

    fn content() -> PlanContent {
        let manifest = backend_manifest();
        let operation = manifest.execute.typed().unwrap().operation.clone();
        PlanContent {
            record_version: PLAN_RECORD_VERSION,
            plan_id: "plan-1".into(),
            job_id: "deploy-backend".into(),
            job_manifest_digest: job_manifest_digest(&manifest).unwrap(),
            role: ArtifactRole::Backend,
            operation,
            artifact: PlanArtifact {
                identity: identity(),
                integrity: integrity(),
                manifest_sha256: manifest_sha(),
            },
            targets: vec!["PC-a".into(), "pc-A".into()],
            not_before: NOW - 10,
            expires_at: NOW + 3600,
            execution_policy: ExecutionPolicy::SingleUsePerTarget,
        }
    }

    /// The job as it must look once the plan exists: it names the plan.
    fn job_for(c: &PlanContent) -> Manifest {
        let mut m = backend_manifest();
        m.deploy_authorization = Some(DeployAuthorizationRef {
            plan_id: c.plan_id.clone(),
            plan_digest: c.digest(),
        });
        m
    }

    fn record(c: PlanContent, state: PlanState) -> PlanRecord {
        let approved = (state == PlanState::Approved).then(|| "alice".to_owned());
        let revoked = (state == PlanState::Revoked).then(|| "bob".to_owned());
        PlanRecord {
            stored_digest: c.digest(),
            content: c,
            state,
            approved_by: approved,
            revoked_by: revoked,
            applied_targets: vec![],
        }
    }

    fn handler() -> DeployHandler {
        DeployHandler::new(
            TYPED_DEPLOY_CONTRACT_V1,
            OPERATION_DEPLOY_BACKEND,
            TargetOs::Linux,
            TargetArch::Aarch64,
        )
    }

    fn good_facts(pc: &str) -> TargetFacts {
        TargetFacts {
            pc_id: pc.into(),
            registered: true,
            current_agent_version: Some("0.95.0".into()),
            current_os_family: Some("linux".into()),
            evidence: CapabilityEvidence {
                capabilities: Some(DeployCapabilities::from_handlers(&[handler()])),
                agent_version: Some("0.95.0".into()),
                protocols: Some(vec!["legacy".into(), PROTOCOL_V2.into()]),
                keys: Some(vec![SIGNER.into()]),
                os_family: Some("linux".into()),
                received_at: Some(NOW - 30),
            },
        }
    }

    use crate::artifact::TargetOs;

    struct Case {
        manifest: Manifest,
        plan: Option<PlanRecord>,
        actor: DispatchActor,
        scope: RequestedScope,
        facts: Vec<TargetFacts>,
        signer: Option<&'static str>,
        now: i64,
    }

    fn operator() -> DispatchActor {
        DispatchActor::Operator(Principal {
            subject: "carol".into(),
            operator: true,
            exec_feature: true,
        })
    }

    fn case() -> Case {
        let c = content();
        Case {
            manifest: job_for(&c),
            scope: RequestedScope {
                pcs: vec!["pc-A".into(), "PC-a".into()],
                ..Default::default()
            },
            facts: c.targets.iter().map(|t| good_facts(t)).collect(),
            plan: Some(record(c, PlanState::Approved)),
            actor: operator(),
            signer: Some(SIGNER),
            now: NOW,
        }
    }

    impl Case {
        fn run(&self) -> Result<AuthorizedDeployment, DeployRefusal> {
            evaluate(&GateInput {
                manifest: &self.manifest,
                plan: self.plan.as_ref(),
                actor: &self.actor,
                scope: &self.scope,
                targets: &self.facts,
                signer: self.signer,
                now: self.now,
            })
        }
    }

    #[test]
    fn digest_is_pinned_by_a_fixed_fixture() {
        // Changing any rule of the canonical form changes this value and must
        // be a deliberate record_version bump.
        assert_eq!(String::from(content().digest()), PINNED_PLAN_DIGEST);
        assert_eq!(
            String::from(job_manifest_digest(&backend_manifest()).unwrap()),
            PINNED_JOB_DIGEST
        );
    }
    const PINNED_PLAN_DIGEST: &str =
        "ad52a0bbe88b9bfe0f3f9d066ee0f76c35413f53a52164d46e5a852cc1edda12";
    const PINNED_JOB_DIGEST: &str =
        "2467b243915cddd7fa10a8cbbd7fe0830fa9bc09b254847f6b58415681b6348a";

    #[test]
    fn job_digest_ignores_only_the_plan_reference() {
        let mut m = backend_manifest();
        let before = job_manifest_digest(&m).unwrap();
        m.deploy_authorization = Some(DeployAuthorizationRef {
            plan_id: "other".into(),
            plan_digest: ArtifactSha256::of(b"x"),
        });
        assert_eq!(job_manifest_digest(&m).unwrap(), before);
        m.version = "1.0.1".into();
        assert_ne!(job_manifest_digest(&m).unwrap(), before);
    }

    #[test]
    fn digest_covers_every_field() {
        let base = content();
        let d = base.digest();
        let mut variants: Vec<PlanContent> = Vec::new();
        let mut v = base.clone();
        v.plan_id = "plan-2".into();
        variants.push(v);
        let mut v = base.clone();
        v.job_id = "other".into();
        variants.push(v);
        let mut v = base.clone();
        v.job_manifest_digest = ArtifactSha256::of(b"j");
        variants.push(v);
        let mut v = base.clone();
        v.targets = vec!["PC-a".into()];
        variants.push(v);
        let mut v = base.clone();
        v.targets = vec!["PC-A".into(), "pc-A".into()];
        variants.push(v);
        let mut v = base.clone();
        v.not_before += 1;
        variants.push(v);
        let mut v = base.clone();
        v.expires_at += 1;
        variants.push(v);
        let mut v = base.clone();
        v.artifact.integrity.size_bytes += 1;
        variants.push(v);
        let mut v = base.clone();
        v.artifact.manifest_sha256 = ArtifactSha256::of(b"m");
        variants.push(v);
        let mut v = base.clone();
        if let TypedOperation::DeployBackend(s) = &mut v.operation {
            s.wipe_db = true;
        }
        variants.push(v);
        for v in variants {
            assert_ne!(v.digest(), d, "{v:?}");
        }
    }

    #[test]
    fn canonical_json_sorts_keys_and_is_compact() {
        let v: Value = serde_json::from_str(r#"{"b":[1,{"z":1,"a":null}],"a":"x"}"#).unwrap();
        assert_eq!(
            String::from_utf8(canonical_json(&v)).unwrap(),
            r#"{"a":"x","b":[1,{"a":null,"z":1}]}"#
        );
    }

    #[test]
    fn structural_validation_rejects_bad_plans() {
        assert!(content().validate().is_ok());
        let mut c = content();
        c.targets = vec!["b".into(), "a".into()];
        assert!(c.validate().is_err(), "unsorted");
        let mut c = content();
        c.targets = vec!["a".into(), "a".into()];
        assert!(c.validate().is_err(), "duplicate");
        assert!(PlanContent::normalize_targets(&["a".into(), "a".into()]).is_err());
        let mut c = content();
        c.targets.clear();
        assert!(c.validate().is_err(), "empty");
        let mut c = content();
        c.expires_at = c.not_before + MAX_PLAN_VALIDITY_SECS + 1;
        assert!(c.validate().is_err(), "unbounded validity");
        let mut c = content();
        c.expires_at = c.not_before;
        assert!(c.validate().is_err(), "empty window");
        let mut c = content();
        c.role = ArtifactRole::Agent;
        assert!(c.validate().unwrap_err().contains("self-update"));
        let mut c = content();
        c.record_version = 2;
        assert!(c.validate().is_err());
        let mut c = content();
        c.artifact.integrity.size_bytes += 1;
        assert!(c.validate().is_err(), "artifact differs from the operation");
    }

    #[test]
    fn a_fully_valid_case_is_authorized_but_dispatch_still_refuses() {
        let c = case();
        let ok = c.run().expect("all checks pass");
        assert_eq!(ok.targets, vec!["PC-a".to_owned(), "pc-A".to_owned()]);
        let input = GateInput {
            manifest: &c.manifest,
            plan: c.plan.as_ref(),
            actor: &c.actor,
            scope: &c.scope,
            targets: &c.facts,
            signer: c.signer,
            now: c.now,
        };
        assert_eq!(dispatch(&input), Err(DeployRefusal::HandlerUnavailable));
    }

    #[test]
    fn plan_state_and_time_gates() {
        let mut c = case();
        c.plan.as_mut().unwrap().state = PlanState::Pending;
        c.plan.as_mut().unwrap().approved_by = None;
        assert_eq!(c.run(), Err(DeployRefusal::NotApproved));

        let mut c = case();
        let p = c.plan.as_mut().unwrap();
        p.state = PlanState::Revoked;
        p.approved_by = None;
        p.revoked_by = Some("x".into());
        assert_eq!(c.run(), Err(DeployRefusal::Revoked));

        let mut c = case();
        c.now = NOW + 3600;
        assert_eq!(c.run(), Err(DeployRefusal::Expired));
        let mut c = case();
        c.now = NOW - 11;
        assert_eq!(c.run(), Err(DeployRefusal::NotYetValid));
        let mut c = case();
        c.plan.as_mut().unwrap().content.not_before = NOW + 5;
        // not_before also changes the digest, so the stored one is now wrong.
        assert!(matches!(c.run(), Err(DeployRefusal::PlanIntegrity(_))));
    }

    #[test]
    fn ambiguous_approval_state_is_refused() {
        let mut c = case();
        c.plan.as_mut().unwrap().approved_by = None;
        assert!(matches!(c.run(), Err(DeployRefusal::AmbiguousState(_))));
        let mut c = case();
        c.plan.as_mut().unwrap().revoked_by = Some("x".into());
        assert!(matches!(c.run(), Err(DeployRefusal::AmbiguousState(_))));
        let mut c = case();
        let p = c.plan.as_mut().unwrap();
        p.state = PlanState::Pending;
        p.approved_by = Some("x".into());
        assert!(matches!(c.run(), Err(DeployRefusal::AmbiguousState(_))));
    }

    #[test]
    fn altered_plan_job_artifact_or_digest_is_refused() {
        // Stored digest no longer matches content.
        let mut c = case();
        c.plan.as_mut().unwrap().stored_digest = ArtifactSha256::of(b"forged");
        assert!(matches!(c.run(), Err(DeployRefusal::PlanIntegrity(_))));

        // A different job body than the approved one.
        let mut c = case();
        c.manifest.version = "9.9.9".into();
        assert!(matches!(c.run(), Err(DeployRefusal::JobMismatch(_))));

        // The job points at another digest.
        let mut c = case();
        c.manifest
            .deploy_authorization
            .as_mut()
            .unwrap()
            .plan_digest = ArtifactSha256::of(b"z");
        assert!(matches!(c.run(), Err(DeployRefusal::JobMismatch(_))));

        // Operation options changed in the job (still a valid job).
        let mut c = case();
        if let crate::typed_operation::ManifestExecute::Typed(t) = &mut c.manifest.execute
            && let TypedOperation::DeployBackend(s) = &mut t.operation
        {
            s.wipe_db = true;
        }
        assert!(matches!(c.run(), Err(DeployRefusal::JobMismatch(_))));

        // Missing plan.
        let mut c = case();
        c.plan = None;
        assert!(matches!(c.run(), Err(DeployRefusal::PlanNotFound(_))));

        // A shell job never reaches plan evaluation.
        let mut c = case();
        c.manifest = crate::strict::from_yaml_str(
            "id: j\nversion: 1.0.0\nexecute:\n  shell: sh\n  script: x\n  timeout: 30s\n",
        )
        .unwrap();
        assert!(matches!(c.run(), Err(DeployRefusal::InvalidJob(_))));
    }

    #[test]
    fn authorization_is_the_callers_current_one() {
        for p in [
            Principal {
                subject: "v".into(),
                operator: false,
                exec_feature: true,
            },
            Principal {
                subject: "v".into(),
                operator: true,
                exec_feature: false,
            },
            Principal {
                subject: "".into(),
                operator: true,
                exec_feature: true,
            },
        ] {
            let mut c = case();
            c.actor = DispatchActor::Operator(p);
            assert_eq!(c.run(), Err(DeployRefusal::NotAuthorized));
        }
        let mut c = case();
        c.actor = DispatchActor::Scheduler;
        assert!(matches!(
            c.run(),
            Err(DeployRefusal::NoDelegatedAuthorization(_))
        ));
        for b in [
            Boundary::Klp,
            Boundary::AgentScheduler,
            Boundary::LocalDispatch,
            Boundary::Cli,
        ] {
            let mut c = case();
            c.actor = DispatchActor::Local(b);
            assert_eq!(c.run(), Err(local_refusal(b)));
        }
    }

    #[test]
    fn target_set_must_match_exactly_and_case_sensitively() {
        let mut c = case();
        c.scope.pcs = vec!["pc-a".into(), "PC-a".into()];
        assert!(matches!(c.run(), Err(DeployRefusal::TargetMismatch(_))));
        let mut c = case();
        c.scope.pcs = vec!["PC-a".into()];
        assert!(matches!(c.run(), Err(DeployRefusal::TargetMismatch(_))));
        let mut c = case();
        c.scope.pcs.push("PC-extra".into());
        assert!(matches!(c.run(), Err(DeployRefusal::TargetMismatch(_))));
        let mut c = case();
        c.scope.pcs.push("PC-a".into());
        assert!(
            matches!(c.run(), Err(DeployRefusal::TargetMismatch(_))),
            "duplicates"
        );
        for scope in [
            RequestedScope {
                all: true,
                ..case().scope
            },
            RequestedScope {
                groups: true,
                ..case().scope
            },
            RequestedScope {
                rollout: true,
                ..case().scope
            },
        ] {
            let mut c = case();
            c.scope = scope;
            assert!(matches!(c.run(), Err(DeployRefusal::TargetMismatch(_))));
        }
    }

    #[test]
    fn replay_is_refused() {
        let mut c = case();
        c.plan.as_mut().unwrap().applied_targets = vec!["pc-A".into()];
        assert_eq!(c.run(), Err(DeployRefusal::Replay("pc-A".into())));
    }

    fn pre(mutate: impl FnOnce(&mut TargetFacts)) -> Result<(), PreflightFailure> {
        let mut f = good_facts("PC-a");
        mutate(&mut f);
        preflight_target(&identity(), OPERATION_DEPLOY_BACKEND, &f, Some(SIGNER), NOW)
    }

    #[test]
    fn preflight_fails_closed_on_absent_empty_stale_and_unknown_evidence() {
        assert_eq!(pre(|_| {}), Ok(()));
        assert_eq!(
            pre(|f| f.registered = false),
            Err(PreflightFailure::NotRegistered)
        );
        assert_eq!(
            pre(|f| f.evidence.capabilities = None),
            Err(PreflightFailure::CapabilitiesUnknown)
        );
        assert_eq!(
            pre(|f| f.evidence.capabilities = Some(DeployCapabilities::from_handlers(&[]))),
            Err(PreflightFailure::CapabilitiesEmpty)
        );
        assert_eq!(
            pre(|f| f.evidence.received_at = None),
            Err(PreflightFailure::EvidenceStale)
        );
        assert_eq!(
            pre(|f| f.evidence.received_at = Some(NOW - MAX_CAPABILITY_EVIDENCE_AGE_SECS - 1)),
            Err(PreflightFailure::EvidenceStale)
        );
        assert_eq!(
            pre(|f| f.evidence.received_at = Some(NOW + 1)),
            Err(PreflightFailure::EvidenceStale),
            "evidence from the future is not fresh"
        );
        assert_eq!(
            pre(|f| f.current_agent_version = Some("0.96.0".into())),
            Err(PreflightFailure::AgentVersionChanged)
        );
        assert_eq!(
            pre(|f| f.current_agent_version = None),
            Err(PreflightFailure::AgentVersionChanged)
        );
        assert!(matches!(
            pre(|f| f.current_os_family = Some("windows".into())),
            Err(PreflightFailure::OsMismatch { .. })
        ));
        assert!(matches!(
            pre(|f| f.current_os_family = None),
            Err(PreflightFailure::OsMismatch { .. })
        ));
        assert!(matches!(
            pre(|f| f.evidence.os_family = Some("macos".into())),
            Err(PreflightFailure::OsMismatch { .. })
        ));
        // A handler for the wrong arch / operation does not satisfy it.
        let other = DeployHandler::new(
            TYPED_DEPLOY_CONTRACT_V1,
            OPERATION_DEPLOY_BACKEND,
            TargetOs::Linux,
            TargetArch::X86_64,
        );
        assert!(matches!(
            pre(|f| f.evidence.capabilities = Some(DeployCapabilities::from_handlers(&[other]))),
            Err(PreflightFailure::NoMatchingHandler { .. })
        ));
        assert_eq!(
            pre(|f| f.evidence.protocols = None),
            Err(PreflightFailure::ProtocolsUnknown)
        );
        assert_eq!(
            pre(|f| f.evidence.protocols = Some(vec![])),
            Err(PreflightFailure::ProtocolMissing)
        );
        assert_eq!(
            pre(|f| f.evidence.protocols = Some(vec!["legacy".into()])),
            Err(PreflightFailure::ProtocolMissing)
        );
        assert_eq!(
            pre(|f| f.evidence.keys = None),
            Err(PreflightFailure::KeysUnknown)
        );
        assert_eq!(
            pre(|f| f.evidence.keys = Some(vec![])),
            Err(PreflightFailure::KeyMismatch)
        );
        // Same kid, different fingerprint.
        assert_eq!(
            pre(|f| f.evidence.keys = Some(vec!["backend-1:ffff".into()])),
            Err(PreflightFailure::KeyMismatch)
        );
        let f = good_facts("PC-a");
        assert_eq!(
            preflight_target(&identity(), OPERATION_DEPLOY_BACKEND, &f, None, NOW),
            Err(PreflightFailure::SignerUnavailable)
        );
    }

    #[test]
    fn gate_surfaces_preflight_failure_for_the_failing_target() {
        let mut c = case();
        c.facts[1].evidence.capabilities = None;
        match c.run() {
            Err(DeployRefusal::Preflight { pc_id, reason }) => {
                assert_eq!(pc_id, "pc-A");
                assert_eq!(reason, PreflightFailure::CapabilitiesUnknown);
            }
            other => panic!("{other:?}"),
        }
        let mut c = case();
        c.facts.pop();
        assert!(matches!(
            c.run(),
            Err(DeployRefusal::Preflight {
                reason: PreflightFailure::NotRegistered,
                ..
            })
        ));
    }
}
