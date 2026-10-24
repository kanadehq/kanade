//! Backend wiring of the shared deployment gate (`kanade_shared::deploy_plan`).
//!
//! The decision logic is pure and lives in the shared crate; this module only
//! gathers what it needs from the database — the stored plan, the per-target
//! capability evidence, the backend's signing identity — and maps a refusal to
//! an HTTP status. Every backend boundary that could deliver a typed job (the
//! direct exec endpoint and the scheduler, which both pass through
//! `exec_manifest`) calls [`refuse_typed`], and so does nothing else: there is
//! one policy.
//!
//! The gate ends in a handler-unavailable barrier, so [`refuse_typed`] returns
//! an error for every input in production. It deliberately has no success
//! value: nothing is published, consumed or recorded as applied.

use axum::http::StatusCode;
use kanade_shared::deploy_plan::{
    CapabilityEvidence, DeployRefusal, DispatchActor, GateInput, PlanRecord, RequestedScope,
    TargetFacts,
};
use kanade_shared::manifest::{FanoutPlan, Manifest};
use kanade_shared::wire::DeployCapabilities;
use sqlx::{Row, SqlitePool};

use crate::api::deploy_plans::{self, LoadError};

/// HTTP status for a refusal. Authorization failures are 403, state conflicts
/// 409, everything else a 422 (the request is understood but not deliverable).
pub fn status_of(r: &DeployRefusal) -> StatusCode {
    match r {
        DeployRefusal::NotAuthorized | DeployRefusal::NoDelegatedAuthorization(_) => {
            StatusCode::FORBIDDEN
        }
        DeployRefusal::Revoked | DeployRefusal::Replay(_) | DeployRefusal::AmbiguousState(_) => {
            StatusCode::CONFLICT
        }
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

/// The scope an exec request names, before any resolution.
pub fn scope_of(plan: &FanoutPlan) -> RequestedScope {
    RequestedScope {
        pcs: plan.target.pcs.clone(),
        all: plan.target.all,
        groups: !plan.target.groups.is_empty(),
        rollout: plan.rollout.is_some(),
    }
}

fn parse_list(raw: Option<String>) -> Option<Vec<String>> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
}

/// Current facts for each requested target. A target without an `agents` row
/// (exact, case-sensitive pc_id) is reported as unregistered.
pub async fn load_target_facts(
    pool: &SqlitePool,
    pcs: &[String],
) -> Result<Vec<TargetFacts>, sqlx::Error> {
    let mut out = Vec::with_capacity(pcs.len());
    for pc in pcs {
        let row = sqlx::query(
            "SELECT agent_version, os_family, agent_arch, deploy_evidence_agent_arch, deploy_evidence_reported_at, deploy_capabilities, deploy_evidence_received_at, \
             deploy_evidence_agent_version, deploy_evidence_protocols, deploy_evidence_keys, \
             deploy_evidence_os_family FROM agents WHERE pc_id = ?",
        )
        .bind(pc)
        .fetch_optional(pool)
        .await?;
        let facts = match row {
            None => TargetFacts {
                pc_id: pc.clone(),
                registered: false,
                current_agent_version: None,
                current_os_family: None,
                current_agent_arch: None,
                evidence: CapabilityEvidence::default(),
            },
            Some(r) => TargetFacts {
                pc_id: pc.clone(),
                registered: true,
                current_agent_version: r.get("agent_version"),
                current_os_family: r.get("os_family"),
                current_agent_arch: r.get("agent_arch"),
                evidence: CapabilityEvidence {
                    // A malformed blob reads as "never reported", not as a
                    // capability set.
                    capabilities: r
                        .get::<Option<String>, _>("deploy_capabilities")
                        .and_then(|s| serde_json::from_str::<DeployCapabilities>(&s).ok()),
                    agent_version: r.get("deploy_evidence_agent_version"),
                    protocols: parse_list(r.get("deploy_evidence_protocols")),
                    keys: parse_list(r.get("deploy_evidence_keys")),
                    os_family: r.get("deploy_evidence_os_family"),
                    agent_arch: r.get("deploy_evidence_agent_arch"),
                    reported_at_ms: r.get("deploy_evidence_reported_at"),
                    received_at: r.get("deploy_evidence_received_at"),
                },
            },
        };
        out.push(facts);
    }
    Ok(out)
}

/// Run the shared gate for a typed job at a backend dispatch boundary and
/// return the refusal. `signer` is `kid:fingerprint` of the backend's current
/// command-signing key.
pub async fn refuse_typed(
    pool: &SqlitePool,
    signer: Option<&str>,
    manifest: &Manifest,
    plan: &FanoutPlan,
    actor: &DispatchActor,
    now: i64,
) -> DeployRefusal {
    let record: Option<PlanRecord> = match manifest.deploy_authorization.as_ref() {
        Some(r) => match deploy_plans::load_record(pool, &r.plan_id).await {
            Ok(rec) => Some(rec),
            Err(LoadError::Missing) => None,
            Err(LoadError::Corrupt(m)) => return DeployRefusal::PlanIntegrity(m),
            Err(LoadError::Storage) => {
                return DeployRefusal::PlanIntegrity("plan storage unavailable".into());
            }
        },
        None => None,
    };
    let scope = scope_of(plan);
    let facts = match load_target_facts(pool, &scope.pcs).await {
        Ok(f) => f,
        Err(_) => {
            return DeployRefusal::PlanIntegrity("agent evidence storage unavailable".into());
        }
    };
    let input = GateInput {
        manifest,
        plan: record.as_ref(),
        actor,
        scope: &scope,
        targets: &facts,
        signer,
        now,
    };
    match kanade_shared::deploy_plan::dispatch(&input) {
        Ok(_) => DeployRefusal::HandlerUnavailable,
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::deploy_plans::test_support::*;
    use crate::api::deploy_plans::{ApprovePlan, approve_plan, create_plan, revoke_plan};
    use kanade_shared::artifact::{ArtifactSha256, TargetArch, TargetOs};
    use kanade_shared::deploy_plan::{Boundary, PreflightFailure, Principal};
    use kanade_shared::wire::{
        DeployHandler, Heartbeat, OPERATION_DEPLOY_BACKEND, PROTOCOL_V2, TYPED_DEPLOY_CONTRACT_V1,
    };

    const SIGNER: &str = "backend-1:abcd";

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn handler() -> DeployHandler {
        DeployHandler::new(
            TYPED_DEPLOY_CONTRACT_V1,
            OPERATION_DEPLOY_BACKEND,
            TargetOs::Linux,
            TargetArch::Aarch64,
        )
    }

    fn beat(pc: &str, caps: Option<DeployCapabilities>) -> Heartbeat {
        Heartbeat {
            agent_arch: Some("aarch64".into()),
            remote_viewing: None,
            pc_id: pc.into(),
            at: chrono::Utc::now(),
            agent_version: "0.95.0".into(),
            hostname: None,
            os_family: Some("linux".into()),
            agent_cpu_pct: None,
            agent_rss_bytes: None,
            agent_disk_read_bytes: None,
            agent_disk_written_bytes: None,
            quarantined_versions: Vec::new(),
            last_logon_user: None,
            last_logon_display_name: None,
            command_keys: Some(vec![SIGNER.into()]),
            enforcing: Some(true),
            command_protocols: Some(vec!["legacy".into(), PROTOCOL_V2.into()]),
            deploy_capabilities: caps,
        }
    }

    async fn project(pool: &sqlx::SqlitePool, hb: &Heartbeat) {
        crate::projector::heartbeat::upsert_baseline(pool, hb)
            .await
            .unwrap();
    }

    fn supporting() -> Option<DeployCapabilities> {
        Some(DeployCapabilities::from_handlers(&[handler()]))
    }

    async fn all_report(pool: &sqlx::SqlitePool, caps: Option<DeployCapabilities>) {
        for pc in ["PC-a", "pc-A"] {
            project(pool, &beat(pc, caps.clone())).await;
        }
    }

    fn operator_actor() -> DispatchActor {
        DispatchActor::Operator(Principal {
            subject: "carol".into(),
            operator: true,
            exec_feature: true,
        })
    }

    fn fan(pcs: &[&str]) -> FanoutPlan {
        serde_json::from_value(serde_json::json!({"target": {"pcs": pcs}})).unwrap()
    }

    /// Approved plan "plan-1" for PC-a and pc-A; returns the job that names it.
    async fn approved(pool: &sqlx::SqlitePool) -> Manifest {
        let n = now();
        let req = create_request(request("plan-1", &["PC-a", "pc-A"], n));
        let view = create_plan(pool, "alice", &req, n).await.unwrap();
        let digest = ArtifactSha256::try_from(view.plan_digest.clone()).unwrap();
        approve_plan(
            pool,
            "plan-1",
            "bob",
            &ApprovePlan {
                plan_digest: digest,
                revision: 0,
            },
            n,
        )
        .await
        .unwrap();
        manifest_of(&job_json(Some(("plan-1", &digest))))
    }

    async fn gate(
        pool: &sqlx::SqlitePool,
        m: &Manifest,
        plan: &FanoutPlan,
        actor: &DispatchActor,
    ) -> DeployRefusal {
        refuse_typed(pool, Some(SIGNER), m, plan, actor, now()).await
    }

    fn preflight(r: DeployRefusal) -> PreflightFailure {
        match r {
            DeployRefusal::Preflight { reason, .. } => reason,
            other => panic!("expected a preflight refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_approved_compatible_plan_still_ends_at_handler_unavailable() {
        let pool = pool().await;
        let job = approved(&pool).await;
        all_report(&pool, supporting()).await;
        let r = gate(&pool, &job, &fan(&["pc-A", "PC-a"]), &operator_actor()).await;
        assert_eq!(r, DeployRefusal::HandlerUnavailable);
        assert_eq!(status_of(&r), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.to_string().contains("not enabled"));
        // Nothing was consumed or recorded.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy_plan_applications")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let (state, rev): (String, i64) =
            sqlx::query_as("SELECT state, revision FROM deploy_plans")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((state.as_str(), rev), ("approved", 1));
    }

    #[tokio::test]
    async fn capability_evidence_none_empty_stale_and_unknown_fail_closed() {
        let pool = pool().await;
        let job = approved(&pool).await;
        let plan = fan(&["PC-a", "pc-A"]);
        let actor = operator_actor();

        // Never reported (older agent): unknown.
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::CapabilitiesUnknown
        );
        // Reported explicitly empty: supports nothing.
        all_report(&pool, Some(DeployCapabilities::from_handlers(&[]))).await;
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::CapabilitiesEmpty
        );
        // A handler for a different arch is not support.
        let other = DeployHandler::new(
            TYPED_DEPLOY_CONTRACT_V1,
            OPERATION_DEPLOY_BACKEND,
            TargetOs::Linux,
            TargetArch::X86_64,
        );
        all_report(&pool, Some(DeployCapabilities::from_handlers(&[other]))).await;
        assert!(matches!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::NoMatchingHandler { .. }
        ));
        // A token this build does not know is not support.
        let unknown: DeployCapabilities = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "handlers": [{"contract": "typed_deploy_v9", "operation": "deploy_backend",
                          "os": "linux", "arch": "aarch64"}]
        }))
        .unwrap();
        all_report(&pool, Some(unknown)).await;
        assert!(matches!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::NoMatchingHandler { .. }
        ));
        // Supported, but stale: receipt freshness is independently required.
        all_report(&pool, supporting()).await;
        assert_eq!(
            gate(&pool, &job, &plan, &actor).await,
            DeployRefusal::HandlerUnavailable
        );
        sqlx::query(
            "UPDATE agents SET deploy_evidence_received_at = deploy_evidence_received_at - 3600",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::EvidenceStale
        );
        // A malformed stored blob reads as never reported, not as support.
        all_report(&pool, supporting()).await;
        sqlx::query("UPDATE agents SET deploy_capabilities = 'not json'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::CapabilitiesUnknown
        );
    }

    #[tokio::test]
    async fn report_time_and_independent_architecture_are_required() {
        let pool = pool().await;
        let job = approved(&pool).await;
        let plan = fan(&["PC-a", "pc-A"]);
        let actor = operator_actor();
        // A delayed heartbeat is newly received but its facts are already old.
        for pc in ["PC-a", "pc-A"] {
            let mut delayed = beat(pc, supporting());
            delayed.at -= chrono::Duration::seconds(3600);
            project(&pool, &delayed).await;
        }
        let facts = load_target_facts(&pool, &["PC-a".into()]).await.unwrap();
        assert!(now() - facts[0].evidence.received_at.unwrap() < 300);
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::EvidenceStale
        );
        all_report(&pool, supporting()).await;
        for arch in [None, Some("x86_64"), Some("unknown")] {
            let mut hb = beat("PC-a", supporting());
            hb.agent_arch = arch.map(str::to_owned);
            project(&pool, &hb).await;
            assert!(matches!(
                preflight(gate(&pool, &job, &plan, &actor).await),
                PreflightFailure::ArchMismatch { .. }
            ));
        }
        project(&pool, &beat("PC-a", supporting())).await;
        let mut ping = beat("PC-a", None);
        ping.agent_arch = Some("x86_64".into());
        project(&pool, &ping).await;
        let facts = load_target_facts(&pool, &["PC-a".into()]).await.unwrap();
        assert_eq!(facts[0].evidence.agent_arch.as_deref(), Some("aarch64"));
        assert_eq!(facts[0].current_agent_arch.as_deref(), Some("x86_64"));
        assert!(matches!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::ArchMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn omitted_reports_neither_refresh_nor_erase_and_version_changes_invalidate() {
        let pool = pool().await;
        let job = approved(&pool).await;
        let plan = fan(&["PC-a", "pc-A"]);
        let actor = operator_actor();
        all_report(&pool, supporting()).await;
        sqlx::query(
            "UPDATE agents SET deploy_evidence_received_at = deploy_evidence_received_at - 3600",
        )
        .execute(&pool)
        .await
        .unwrap();
        // A ping-shaped heartbeat (no capabilities) must not refresh freshness…
        all_report(&pool, None).await;
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::EvidenceStale
        );
        // …nor erase the stored report.
        let stored: Option<String> =
            sqlx::query_scalar("SELECT deploy_capabilities FROM agents WHERE pc_id = 'PC-a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stored.is_some());

        // Fresh evidence, then the agent upgrades and a ping (no caps) arrives:
        // the old evidence belongs to the old version and is not reused.
        all_report(&pool, supporting()).await;
        assert_eq!(
            gate(&pool, &job, &plan, &actor).await,
            DeployRefusal::HandlerUnavailable
        );
        for pc in ["PC-a", "pc-A"] {
            let mut hb = beat(pc, None);
            hb.agent_version = "0.96.0".into();
            project(&pool, &hb).await;
        }
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::AgentVersionChanged
        );
        // A real report from the new version restores it.
        for pc in ["PC-a", "pc-A"] {
            let mut hb = beat(pc, supporting());
            hb.agent_version = "0.96.0".into();
            project(&pool, &hb).await;
        }
        assert_eq!(
            gate(&pool, &job, &plan, &actor).await,
            DeployRefusal::HandlerUnavailable
        );
    }

    #[tokio::test]
    async fn an_out_of_order_older_report_cannot_roll_evidence_back() {
        let pool = pool().await;
        let fresh = beat("PC-a", supporting());
        let mut older = beat("PC-a", Some(DeployCapabilities::from_handlers(&[])));
        older.at = fresh.at - chrono::Duration::seconds(60);
        project(&pool, &fresh).await;
        project(&pool, &older).await;
        let caps: String =
            sqlx::query_scalar("SELECT deploy_capabilities FROM agents WHERE pc_id = 'PC-a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let caps: DeployCapabilities = serde_json::from_str(&caps).unwrap();
        assert_eq!(caps.handlers.len(), 1, "the newer report stands");
    }

    #[tokio::test]
    async fn command_protocol_key_and_os_mismatches_fail_closed() {
        let pool = pool().await;
        let job = approved(&pool).await;
        let plan = fan(&["PC-a", "pc-A"]);
        let actor = operator_actor();

        let mut hb = beat("PC-a", supporting());
        hb.command_protocols = Some(vec!["legacy".into()]);
        project(&pool, &hb).await;
        project(&pool, &beat("pc-A", supporting())).await;
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::ProtocolMissing
        );
        let mut hb = beat("PC-a", supporting());
        hb.command_protocols = None;
        project(&pool, &hb).await;
        assert_eq!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::ProtocolsUnknown
        );
        // Right kid, wrong fingerprint; empty ring; unreported ring.
        for (keys, expect) in [
            (
                Some(vec!["backend-1:ffff".to_string()]),
                PreflightFailure::KeyMismatch,
            ),
            (Some(vec![]), PreflightFailure::KeyMismatch),
            (None, PreflightFailure::KeysUnknown),
        ] {
            let mut hb = beat("PC-a", supporting());
            hb.command_keys = keys;
            project(&pool, &hb).await;
            assert_eq!(preflight(gate(&pool, &job, &plan, &actor).await), expect);
        }
        // The backend itself cannot sign.
        project(&pool, &beat("PC-a", supporting())).await;
        let r = refuse_typed(&pool, None, &job, &plan, &actor, now()).await;
        assert_eq!(preflight(r), PreflightFailure::SignerUnavailable);
        // Wrong OS family.
        let mut hb = beat("PC-a", supporting());
        hb.os_family = Some("windows".into());
        project(&pool, &hb).await;
        assert!(matches!(
            preflight(gate(&pool, &job, &plan, &actor).await),
            PreflightFailure::OsMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn cross_entrypoint_bypass_attempts_are_refused() {
        let pool = pool().await;
        let job = approved(&pool).await;
        all_report(&pool, supporting()).await;
        let actor = operator_actor();

        // Scheduler and every local path: no delegated authorization.
        let r = gate(
            &pool,
            &job,
            &fan(&["PC-a", "pc-A"]),
            &DispatchActor::Scheduler,
        )
        .await;
        assert_eq!(r, DeployRefusal::NoDelegatedAuthorization("the scheduler"));
        for b in [
            Boundary::Klp,
            Boundary::AgentScheduler,
            Boundary::LocalDispatch,
            Boundary::Cli,
        ] {
            let r = gate(
                &pool,
                &job,
                &fan(&["PC-a", "pc-A"]),
                &DispatchActor::Local(b),
            )
            .await;
            assert_eq!(r, kanade_shared::deploy_plan::local_refusal(b));
            assert_eq!(status_of(&r), StatusCode::FORBIDDEN);
        }
        // An operator who lost operator standing / exec permission.
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
                subject: String::new(),
                operator: true,
                exec_feature: true,
            },
        ] {
            let r = gate(
                &pool,
                &job,
                &fan(&["PC-a", "pc-A"]),
                &DispatchActor::Operator(p),
            )
            .await;
            assert_eq!(r, DeployRefusal::NotAuthorized);
        }
        // Scope self-expansion or narrowing: all / group / rollout / other pcs.
        let mut all = fan(&["PC-a", "pc-A"]);
        all.target.all = true;
        let mut grp = fan(&["PC-a", "pc-A"]);
        grp.target.groups = vec!["fleet".into()];
        for p in [
            all,
            grp,
            fan(&["PC-a"]),
            fan(&["PC-a", "pc-A", "PC-extra"]),
            fan(&["pc-a", "pc-A"]),
            fan(&["PC-a", "PC-a", "pc-A"]),
        ] {
            let r = gate(&pool, &job, &p, &actor).await;
            assert!(matches!(r, DeployRefusal::TargetMismatch(_)), "{r:?}");
        }
        // A different (valid) job body than the approved one.
        let mut other = job.clone();
        other.version = "9.9.9".into();
        let r = gate(&pool, &other, &fan(&["PC-a", "pc-A"]), &actor).await;
        assert!(matches!(r, DeployRefusal::JobMismatch(_)), "{r:?}");
        // A job that names a plan that does not exist.
        let ghost = manifest_of(&job_json(Some(("ghost", &ArtifactSha256::of(b"g")))));
        let r = gate(&pool, &ghost, &fan(&["PC-a", "pc-A"]), &actor).await;
        assert!(matches!(r, DeployRefusal::PlanNotFound(_)), "{r:?}");
        // A shell job is not the gate's concern and is never typed-refused here.
        let shell = manifest_of(&serde_json::json!({"id": "j", "version": "1.0.0",
            "execute": {"shell": "sh", "script": "x", "timeout": "30s"}}));
        let r = gate(&pool, &shell, &fan(&["PC-a"]), &actor).await;
        assert!(matches!(r, DeployRefusal::InvalidJob(_)));
    }

    #[tokio::test]
    async fn unapproved_revoked_expired_and_replayed_plans_are_refused() {
        let n = now();
        // Pending (never approved).
        let pool = pool().await;
        let req = create_request(request("plan-1", &["PC-a", "pc-A"], n));
        let view = create_plan(&pool, "alice", &req, n).await.unwrap();
        let digest = ArtifactSha256::try_from(view.plan_digest).unwrap();
        let job = manifest_of(&job_json(Some(("plan-1", &digest))));
        all_report(&pool, supporting()).await;
        let plan = fan(&["PC-a", "pc-A"]);
        let actor = operator_actor();
        assert_eq!(
            gate(&pool, &job, &plan, &actor).await,
            DeployRefusal::NotApproved
        );

        // Approved, then replayed (ledger rows exist).
        approve_plan(
            &pool,
            "plan-1",
            "bob",
            &ApprovePlan {
                plan_digest: digest,
                revision: 0,
            },
            n,
        )
        .await
        .unwrap();
        assert_eq!(
            gate(&pool, &job, &plan, &actor).await,
            DeployRefusal::HandlerUnavailable
        );
        sqlx::query("INSERT INTO deploy_plan_applications VALUES ('plan-1','pc-A','e',1)")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            gate(&pool, &job, &plan, &actor).await,
            DeployRefusal::Replay("pc-A".into())
        );
        sqlx::query("DELETE FROM deploy_plan_applications")
            .execute(&pool)
            .await
            .unwrap();

        // Expired: the clock moves past expires_at.
        let r = refuse_typed(&pool, Some(SIGNER), &job, &plan, &actor, n + 7200).await;
        assert_eq!(r, DeployRefusal::Expired);
        // Not yet valid.
        let r = refuse_typed(&pool, Some(SIGNER), &job, &plan, &actor, n - 3600).await;
        assert_eq!(r, DeployRefusal::NotYetValid);

        // Revoked.
        revoke_plan(&pool, "plan-1", "carol", None, n)
            .await
            .unwrap();
        let r = gate(&pool, &job, &plan, &actor).await;
        assert_eq!(r, DeployRefusal::Revoked);
        assert_eq!(status_of(&r), StatusCode::CONFLICT);

        // Tampered content (digest no longer matches).
        let pool2 = self::pool().await;
        let job2 = approved(&pool2).await;
        all_report(&pool2, supporting()).await;
        assert!(
            sqlx::query("UPDATE deploy_plans SET content = replace(content, 'PC-a', 'PC-x')")
                .execute(&pool2)
                .await
                .is_err()
        );
        sqlx::query("DROP TRIGGER deploy_plan_immutable")
            .execute(&pool2)
            .await
            .unwrap();
        sqlx::query("DROP TRIGGER deploy_plan_transition")
            .execute(&pool2)
            .await
            .unwrap();
        sqlx::query("UPDATE deploy_plans SET content = replace(content, 'PC-a', 'PC-x')")
            .execute(&pool2)
            .await
            .unwrap();
        let r = gate(&pool2, &job2, &plan, &actor).await;
        assert!(matches!(r, DeployRefusal::PlanIntegrity(_)), "{r:?}");
    }
}
