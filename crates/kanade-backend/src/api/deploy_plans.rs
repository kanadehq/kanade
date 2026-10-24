//! Deployment plan records: persistence and the authenticated
//! create / read / approve / revoke API (SPEC §2.13.5.6, migration stage 4).
//!
//! A plan is **authorization state**, kept apart from the other two decisions a
//! deployment needs: who published a package (publisher trust) and whether this
//! exact package was admitted (package admission). Registering or approving a
//! plan proves neither, and neither of those proves a human approved the
//! rollout. Approval alone also does not make a typed job deliverable: the
//! dispatch gate (`crate::deploy_gate`) re-checks everything and ends in the
//! handler-unavailable barrier.
//!
//! Rules enforced here:
//!
//! * Plans are immutable. There is no content-edit endpoint; a change is a new
//!   plan with a new id and digest.
//! * Identity and permission come only from the authenticated context
//!   ([`Caller`]); request bodies carry no actor, no `approved` flag and no
//!   state, and unknown request fields are rejected.
//! * Every state change is a single conditional `UPDATE` (state, revision,
//!   digest and validity in the `WHERE`), so concurrent approve / revoke /
//!   consume cannot both win and a stale approval cannot land.
//! * Approval names the digest and revision the approver reviewed; if either
//!   moved, the approval is refused.
//! * The plan digest is recomputed from the stored content on every read that
//!   matters; a record whose columns disagree is reported, never trusted.

use axum::extract::{FromRef, Path, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing::get, routing::post};
use kanade_shared::artifact::ArtifactSha256;
use kanade_shared::deploy_plan::{
    ExecutionPolicy, MAX_PLAN_VALIDITY_SECS, PLAN_RECORD_VERSION, PlanArtifact, PlanContent,
    PlanRecord, PlanState, job_manifest_digest,
};
use kanade_shared::manifest::Manifest;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use super::AppState;
use crate::audit::{self, Caller};

type Error = (StatusCode, String);

/// The NATS handle used only to publish audit events. Optional so the plan API
/// can be exercised (and its router built) over SQLite alone.
#[derive(Clone)]
pub struct AuditSink(pub Option<async_nats::Client>);

impl FromRef<AppState> for AuditSink {
    fn from_ref(state: &AppState) -> Self {
        AuditSink(Some(state.nats.clone()))
    }
}

/// Production routes, also used verbatim by the access-control tests so the two
/// cannot drift. Every route needs at least `operator`.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SqlitePool: FromRef<S>,
    AuditSink: FromRef<S>,
{
    Router::new()
        .route("/api/deploy-plans", get(list).post(create))
        .route("/api/deploy-plans/{plan_id}", get(read))
        .route("/api/deploy-plans/{plan_id}/approve", post(approve))
        .route("/api/deploy-plans/{plan_id}/revoke", post(revoke))
        .route_layer(axum::middleware::from_fn(crate::auth::require_operator))
}

fn err(status: StatusCode, msg: impl Into<String>) -> Error {
    (status, msg.into())
}

fn db_error(e: sqlx::Error) -> Error {
    tracing::warn!(error = %e, "deploy plan storage error");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "deployment plan storage unavailable",
    )
}

/// The authenticated, currently-authorized actor, or a 403.
fn actor(caller: &Caller) -> Result<String, Error> {
    match caller.principal() {
        Some(p) if p.operator && p.exec_feature => Ok(p.subject),
        _ => Err(err(
            StatusCode::FORBIDDEN,
            "an authenticated operator with exec permission is required",
        )),
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `POST /api/deploy-plans`. No actor, no approval flag, no state: the server
/// derives identity and starts every plan `pending`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePlan {
    pub plan_id: String,
    /// The digest the operator computed locally for this exact content. It is
    /// never trusted: the server recomputes it and refuses a mismatch.
    pub plan_digest: ArtifactSha256,
    /// The typed job definition (it must reference this plan id and digest).
    pub job: serde_json::Value,
    pub artifact: PlanArtifact,
    /// Exact registered pc_ids (case-sensitive).
    pub targets: Vec<String>,
    pub not_before: i64,
    pub expires_at: i64,
    pub execution_policy: ExecutionPolicy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovePlan {
    /// The digest the approver reviewed.
    pub plan_digest: ArtifactSha256,
    /// The record revision the approver reviewed.
    pub revision: i64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RevokePlan {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct PlanView {
    pub plan_id: String,
    pub plan_digest: String,
    pub state: PlanState,
    /// `true` once `now` is past `expires_at` (a derived fact, not a state).
    pub expired: bool,
    pub revision: i64,
    pub content: PlanContent,
    pub job: serde_json::Value,
    pub created_by: String,
    pub created_at: i64,
    pub approved_by: Option<String>,
    pub approved_at: Option<i64>,
    pub revoked_by: Option<String>,
    pub revoked_at: Option<i64>,
    pub revoke_reason: Option<String>,
    pub applied_targets: Vec<String>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

struct Row_ {
    plan_digest: String,
    content: String,
    job_manifest: String,
    state: String,
    revision: i64,
    created_by: String,
    created_at: i64,
    approved_by: Option<String>,
    approved_at: Option<i64>,
    revoked_by: Option<String>,
    revoked_at: Option<i64>,
    revoke_reason: Option<String>,
}

fn row_of(r: &SqliteRow) -> Row_ {
    Row_ {
        plan_digest: r.get("plan_digest"),
        content: r.get("content"),
        job_manifest: r.get("job_manifest"),
        state: r.get("state"),
        revision: r.get("revision"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        approved_by: r.get("approved_by"),
        approved_at: r.get("approved_at"),
        revoked_by: r.get("revoked_by"),
        revoked_at: r.get("revoked_at"),
        revoke_reason: r.get("revoke_reason"),
    }
}

fn parse_state(s: &str) -> Option<PlanState> {
    match s {
        "pending" => Some(PlanState::Pending),
        "approved" => Some(PlanState::Approved),
        "revoked" => Some(PlanState::Revoked),
        _ => None,
    }
}

/// Why a stored plan could not be turned into a trustworthy record.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    Missing,
    Corrupt(String),
    Storage,
}

/// Load a plan and its single-use ledger. A record that no longer decodes, or
/// whose stored digest differs from the digest of its stored content, is
/// `Corrupt` — never silently accepted.
pub async fn load_record(pool: &SqlitePool, plan_id: &str) -> Result<PlanRecord, LoadError> {
    let (record, _) = load_full(pool, plan_id).await?;
    Ok(record)
}

async fn load_full(pool: &SqlitePool, plan_id: &str) -> Result<(PlanRecord, Row_), LoadError> {
    let row = sqlx::query("SELECT * FROM deploy_plans WHERE plan_id = ?")
        .bind(plan_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| LoadError::Storage)?
        .ok_or(LoadError::Missing)?;
    let r = row_of(&row);
    let corrupt = |m: &str| LoadError::Corrupt(m.to_owned());
    let content: PlanContent =
        serde_json::from_str(&r.content).map_err(|_| corrupt("plan content does not decode"))?;
    let stored_digest = ArtifactSha256::try_from(r.plan_digest.clone())
        .map_err(|_| corrupt("stored digest is malformed"))?;
    if content.plan_id != plan_id {
        return Err(corrupt("stored content names another plan id"));
    }
    if content.digest() != stored_digest {
        return Err(corrupt("stored digest does not match the stored content"));
    }
    let state = parse_state(&r.state).ok_or_else(|| corrupt("unknown plan state"))?;
    let applied: Vec<String> =
        sqlx::query_scalar("SELECT pc_id FROM deploy_plan_applications WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_all(pool)
            .await
            .map_err(|_| LoadError::Storage)?;
    let record = PlanRecord {
        content,
        stored_digest,
        state,
        approved_by: r.approved_by.clone(),
        revoked_by: r.revoked_by.clone(),
        applied_targets: applied,
    };
    Ok((record, r))
}

fn view_of(record: PlanRecord, r: Row_, now: i64) -> PlanView {
    PlanView {
        plan_id: record.content.plan_id.clone(),
        plan_digest: r.plan_digest,
        state: record.state,
        expired: now >= record.content.expires_at,
        revision: r.revision,
        job: serde_json::from_str(&r.job_manifest).unwrap_or(serde_json::Value::Null),
        created_by: r.created_by,
        created_at: r.created_at,
        approved_by: r.approved_by,
        approved_at: r.approved_at,
        revoked_by: r.revoked_by,
        revoked_at: r.revoked_at,
        revoke_reason: r.revoke_reason,
        applied_targets: record.applied_targets,
        content: record.content,
    }
}

fn load_error(e: LoadError) -> Error {
    match e {
        LoadError::Missing => err(StatusCode::NOT_FOUND, "deploy plan not found"),
        LoadError::Corrupt(m) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("deploy plan record failed its integrity check: {m}"),
        ),
        LoadError::Storage => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "deployment plan storage unavailable",
        ),
    }
}

/// Build and validate the plan a create request describes. Pure.
pub fn content_from_request(req: &CreatePlan, now: i64) -> Result<(PlanContent, Manifest), Error> {
    let bad = |m: String| err(StatusCode::BAD_REQUEST, m);
    let job_bytes = serde_json::to_vec(&req.job).map_err(|e| bad(e.to_string()))?;
    let job: Manifest = kanade_shared::strict::from_json_slice(&job_bytes)
        .map_err(|e| bad(format!("invalid job: {e}")))?;
    let typed = job
        .execute
        .typed()
        .ok_or_else(|| bad("the job must be a typed_operation job".into()))?;
    job.validate()
        .map_err(|e| bad(format!("invalid job: {e}")))?;
    let targets = PlanContent::normalize_targets(&req.targets).map_err(bad)?;
    let content = PlanContent {
        record_version: PLAN_RECORD_VERSION,
        plan_id: req.plan_id.clone(),
        job_id: job.id.clone(),
        job_manifest_digest: job_manifest_digest(&job).map_err(bad)?,
        role: typed.operation.expected_role(),
        operation: typed.operation.clone(),
        artifact: req.artifact.clone(),
        targets,
        not_before: req.not_before,
        expires_at: req.expires_at,
        execution_policy: req.execution_policy,
    };
    content.validate().map_err(bad)?;
    if req.expires_at <= now {
        return Err(bad("the plan is already expired".into()));
    }
    if req.expires_at - now > MAX_PLAN_VALIDITY_SECS {
        return Err(bad(format!(
            "expires_at is more than {MAX_PLAN_VALIDITY_SECS}s in the future"
        )));
    }
    if content.digest() != req.plan_digest {
        return Err(bad(
            "plan_digest does not match the digest of the submitted plan content".into(),
        ));
    }
    // The job must reference exactly this plan, and be the job the plan binds.
    content
        .verify_job(&job)
        .map_err(|e| bad(format!("the job does not match the plan: {e}")))?;
    Ok((content, job))
}

/// Every target must be a registered agent with exactly this pc_id.
async fn check_targets_registered(pool: &SqlitePool, targets: &[String]) -> Result<(), Error> {
    let mut missing = Vec::new();
    for t in targets {
        // `=` on TEXT is BINARY (case-sensitive) in SQLite.
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM agents WHERE pc_id = ?")
            .bind(t)
            .fetch_optional(pool)
            .await
            .map_err(db_error)?;
        if found.is_none() {
            missing.push(t.clone());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "targets are not registered agents (pc_id is matched case-sensitively): {missing:?}"
            ),
        ))
    }
}

/// Register a pending plan. Returns the stored view.
pub async fn create_plan(
    pool: &SqlitePool,
    created_by: &str,
    req: &CreatePlan,
    now: i64,
) -> Result<PlanView, Error> {
    let (content, job) = content_from_request(req, now)?;
    check_targets_registered(pool, &content.targets).await?;
    let canonical = String::from_utf8(kanade_shared::deploy_plan::canonical_json(
        &serde_json::to_value(&content).expect("plan content serializes"),
    ))
    .expect("canonical JSON is UTF-8");
    let job_json = serde_json::to_string(&job).expect("job serializes");
    let digest = String::from(content.digest());
    let role = content.role.as_str();
    let res = sqlx::query(
        "INSERT INTO deploy_plans (plan_id, record_version, plan_digest, content, job_manifest, \
         job_id, role, not_before, expires_at, state, revision, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', 0, ?, ?)",
    )
    .bind(&content.plan_id)
    .bind(PLAN_RECORD_VERSION as i64)
    .bind(&digest)
    .bind(&canonical)
    .bind(&job_json)
    .bind(&content.job_id)
    .bind(role)
    .bind(content.not_before)
    .bind(content.expires_at)
    .bind(created_by)
    .bind(now)
    .execute(pool)
    .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
            return Err(err(
                StatusCode::CONFLICT,
                "a deploy plan with this id already exists (plans are immutable; use a new id)",
            ));
        }
        Err(e) => return Err(db_error(e)),
    }
    let (record, row) = load_full(pool, &content.plan_id)
        .await
        .map_err(load_error)?;
    Ok(view_of(record, row, now))
}

/// Approve a pending plan: one conditional UPDATE keyed on state, digest,
/// revision and validity. On a lost race the row is re-read to say why.
pub async fn approve_plan(
    pool: &SqlitePool,
    plan_id: &str,
    approver: &str,
    req: &ApprovePlan,
    now: i64,
) -> Result<PlanView, Error> {
    // Refuse to approve a record whose stored content no longer hashes to its
    // digest, before looking at anything else.
    let (record, _) = load_full(pool, plan_id).await.map_err(load_error)?;
    record
        .content
        .validate()
        .map_err(|e| err(StatusCode::CONFLICT, format!("stored plan is invalid: {e}")))?;
    let digest = String::from(req.plan_digest);
    let res = sqlx::query(
        "UPDATE deploy_plans SET state = 'approved', approved_by = ?, approved_at = ?, \
         revision = revision + 1 \
         WHERE plan_id = ? AND state = 'pending' AND plan_digest = ? AND revision = ? \
         AND expires_at > ?",
    )
    .bind(approver)
    .bind(now)
    .bind(plan_id)
    .bind(&digest)
    .bind(req.revision)
    .bind(now)
    .execute(pool)
    .await
    .map_err(db_error)?;
    if res.rows_affected() == 1 {
        let (record, row) = load_full(pool, plan_id).await.map_err(load_error)?;
        return Ok(view_of(record, row, now));
    }
    // Explain the refusal from the current row (never from the request).
    let (record, row) = load_full(pool, plan_id).await.map_err(load_error)?;
    Err(match record.state {
        PlanState::Revoked => err(StatusCode::CONFLICT, "the plan was revoked"),
        PlanState::Approved => err(StatusCode::CONFLICT, "the plan is already approved"),
        PlanState::Pending if now >= record.content.expires_at => {
            err(StatusCode::GONE, "the plan expired")
        }
        PlanState::Pending if row.plan_digest != digest => err(
            StatusCode::CONFLICT,
            "the plan_digest differs from the stored plan; review the current plan",
        ),
        PlanState::Pending if row.revision != req.revision => err(
            StatusCode::CONFLICT,
            "the plan revision moved since it was reviewed",
        ),
        PlanState::Pending => err(StatusCode::CONFLICT, "the plan could not be approved"),
    })
}

/// Revoke a pending or approved plan (idempotent refusal if already revoked).
pub async fn revoke_plan(
    pool: &SqlitePool,
    plan_id: &str,
    revoker: &str,
    reason: Option<&str>,
    now: i64,
) -> Result<PlanView, Error> {
    let res = sqlx::query(
        "UPDATE deploy_plans SET state = 'revoked', revoked_by = ?, revoked_at = ?, \
         revoke_reason = ?, revision = revision + 1 \
         WHERE plan_id = ? AND state IN ('pending', 'approved')",
    )
    .bind(revoker)
    .bind(now)
    .bind(reason)
    .bind(plan_id)
    .execute(pool)
    .await
    .map_err(db_error)?;
    let (record, row) = load_full(pool, plan_id).await.map_err(load_error)?;
    if res.rows_affected() == 1 {
        Ok(view_of(record, row, now))
    } else {
        Err(err(StatusCode::CONFLICT, "the plan was already revoked"))
    }
}

/// Why a consume attempt failed.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub enum ConsumeError {
    /// Not approved / revoked / outside its window / digest or revision moved.
    NotConsumable,
    /// At least one (plan, target) pair was already applied.
    Replay,
    Storage,
}

/// Atomically consume an approved plan for its exact target set, recording one
/// ledger row per target. The conditional UPDATE both checks and takes the
/// write lock; the ledger's primary key makes a second application of any
/// (plan, target) pair impossible. Everything rolls back on any failure.
///
/// Nothing in production calls this: dispatch ends in the handler-unavailable
/// barrier before it. It exists, and is tested, so that the single-use and race
/// semantics are settled before a real handler is attached.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn consume_plan(
    pool: &SqlitePool,
    plan_id: &str,
    expected_digest: &ArtifactSha256,
    expected_revision: i64,
    exec_id: &str,
    now: i64,
) -> Result<Vec<String>, ConsumeError> {
    let mut tx = pool.begin().await.map_err(|_| ConsumeError::Storage)?;
    let res = sqlx::query(
        "UPDATE deploy_plans SET revision = revision + 1 \
         WHERE plan_id = ? AND state = 'approved' AND plan_digest = ? AND revision = ? \
         AND not_before <= ? AND expires_at > ?",
    )
    .bind(plan_id)
    .bind(String::from(*expected_digest))
    .bind(expected_revision)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|_| ConsumeError::Storage)?;
    if res.rows_affected() != 1 {
        return Err(ConsumeError::NotConsumable);
    }
    let content: String = sqlx::query_scalar("SELECT content FROM deploy_plans WHERE plan_id = ?")
        .bind(plan_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| ConsumeError::Storage)?;
    let content: PlanContent =
        serde_json::from_str(&content).map_err(|_| ConsumeError::NotConsumable)?;
    if content.digest() != *expected_digest {
        return Err(ConsumeError::NotConsumable);
    }
    for pc in &content.targets {
        let r = sqlx::query(
            "INSERT INTO deploy_plan_applications (plan_id, pc_id, exec_id, applied_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(plan_id)
        .bind(pc)
        .bind(exec_id)
        .bind(now)
        .execute(&mut *tx)
        .await;
        match r {
            Ok(_) => {}
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
                return Err(ConsumeError::Replay);
            }
            Err(_) => return Err(ConsumeError::Storage),
        }
    }
    tx.commit().await.map_err(|_| ConsumeError::Storage)?;
    Ok(content.targets)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

async fn audit_event(sink: &AuditSink, caller: &Caller, who: &str, action: &str, plan: &PlanView) {
    if let Some(nats) = &sink.0 {
        audit::record(
            nats,
            who,
            action,
            Some(&plan.plan_id),
            Some(caller),
            serde_json::json!({
                "plan_digest": plan.plan_digest,
                "state": plan.state,
                "revision": plan.revision,
                "role": plan.content.role,
                "targets": plan.content.targets,
                "expires_at": plan.content.expires_at,
            }),
        )
        .await;
    }
}

pub async fn create(
    State(pool): State<SqlitePool>,
    State(sink): State<AuditSink>,
    caller: Caller,
    Json(req): Json<CreatePlan>,
) -> Result<(StatusCode, Json<PlanView>), Error> {
    let who = actor(&caller)?;
    let view = create_plan(&pool, &who, &req, now()).await?;
    audit_event(&sink, &caller, &who, "deploy_plan.create", &view).await;
    Ok((StatusCode::CREATED, Json(view)))
}

pub async fn list(State(pool): State<SqlitePool>) -> Result<Json<Vec<PlanView>>, Error> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT plan_id FROM deploy_plans ORDER BY created_at DESC, plan_id")
            .fetch_all(&pool)
            .await
            .map_err(db_error)?;
    let n = now();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let (record, row) = load_full(&pool, &id).await.map_err(load_error)?;
        out.push(view_of(record, row, n));
    }
    Ok(Json(out))
}

pub async fn read(
    State(pool): State<SqlitePool>,
    Path(plan_id): Path<String>,
) -> Result<Json<PlanView>, Error> {
    let (record, row) = load_full(&pool, &plan_id).await.map_err(load_error)?;
    Ok(Json(view_of(record, row, now())))
}

pub async fn approve(
    State(pool): State<SqlitePool>,
    State(sink): State<AuditSink>,
    Path(plan_id): Path<String>,
    caller: Caller,
    Json(req): Json<ApprovePlan>,
) -> Result<Json<PlanView>, Error> {
    let who = actor(&caller)?;
    let view = approve_plan(&pool, &plan_id, &who, &req, now()).await?;
    audit_event(&sink, &caller, &who, "deploy_plan.approve", &view).await;
    Ok(Json(view))
}

pub async fn revoke(
    State(pool): State<SqlitePool>,
    State(sink): State<AuditSink>,
    Path(plan_id): Path<String>,
    caller: Caller,
    body: axum::body::Bytes,
) -> Result<Json<PlanView>, Error> {
    let who = actor(&caller)?;
    // The body is optional (`{"reason": ".."}`); an empty one is a plain revoke.
    let reason = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        serde_json::from_slice::<RevokePlan>(&body)
            .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid revoke body: {e}")))?
            .reason
    };
    if reason.as_deref().is_some_and(|r| r.len() > 256) {
        return Err(err(StatusCode::BAD_REQUEST, "reason is at most 256 bytes"));
    }
    let view = revoke_plan(&pool, &plan_id, &who, reason.as_deref(), now()).await?;
    audit_event(&sink, &caller, &who, "deploy_plan.revoke", &view).await;
    Ok(Json(view))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the plan API, store and gate tests: a real SQLite
    //! database with the production migrations, the production plan routes with
    //! the production layers, and a valid typed backend plan.
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use kanade_shared::artifact::{ArtifactIdentity, ArtifactIntegrity};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    pub const NOW: i64 = 1_800_000_000;
    pub const ARTIFACT_BYTES: &[u8] = b"backend package bytes";
    pub const SIGNED_MANIFEST_BYTES: &[u8] = b"signed package manifest bytes";

    #[derive(Clone)]
    pub struct TestState {
        pub pool: SqlitePool,
        pub sink: AuditSink,
    }
    impl FromRef<TestState> for SqlitePool {
        fn from_ref(s: &TestState) -> Self {
            s.pool.clone()
        }
    }
    impl FromRef<TestState> for AuditSink {
        fn from_ref(s: &TestState) -> Self {
            s.sink.clone()
        }
    }

    /// In-memory database (one connection) with the real migrations and agents
    /// `PC-a`, `pc-A` (distinct registered ids that differ only by case).
    pub async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        seed_agents(&pool).await;
        pool
    }

    pub async fn seed_agents(pool: &SqlitePool) {
        for pc in ["PC-a", "pc-A"] {
            sqlx::query("INSERT INTO agents (pc_id, hostname) VALUES (?, ?)")
                .bind(pc)
                .bind(pc)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    pub fn identity() -> ArtifactIdentity {
        serde_json::from_value(serde_json::json!({
            "role": "backend", "version": "0.80.0", "os": "linux",
            "arch": "aarch64", "distro_abi": "musl", "format": "tar_gz"
        }))
        .unwrap()
    }

    pub fn artifact() -> PlanArtifact {
        PlanArtifact {
            identity: identity(),
            integrity: ArtifactIntegrity::of(ARTIFACT_BYTES),
            manifest_sha256: ArtifactSha256::of(SIGNED_MANIFEST_BYTES),
        }
    }

    /// A typed backend job. `auth` is the (plan id, digest) it names.
    pub fn job_json(auth: Option<(&str, &ArtifactSha256)>) -> serde_json::Value {
        let a = artifact();
        let mut job = serde_json::json!({
            "id": "deploy-backend",
            "version": "1.0.0",
            "require_approval": true,
            "execute": {
                "kind": "typed_operation",
                "payload_version": 1,
                "timeout": "10m",
                "run_as": "system",
                "operation": {"type": "deploy_backend", "spec": {
                    "artifact_identity": a.identity,
                    "package_object_key": "backend/0.80.0/linux-aarch64-musl.tar_gz",
                    "artifact_sha256": a.integrity.sha256,
                    "artifact_size_bytes": a.integrity.size_bytes,
                    "package_id": "com.kanade.package",
                    "manifest_sha256": a.manifest_sha256,
                    "preserve_config": true,
                    "require_db_backup": true,
                    "wipe_db": false,
                    "start_policy": "preserve",
                    "firewall": "preserve"
                }}
            }
        });
        if let Some((id, digest)) = auth {
            job["deploy_authorization"] = serde_json::json!({"plan_id": id, "plan_digest": digest});
        }
        job
    }

    pub fn manifest_of(job: &serde_json::Value) -> Manifest {
        kanade_shared::strict::from_json_slice(&serde_json::to_vec(job).unwrap()).unwrap()
    }

    /// The plan content a correct client would build for these inputs.
    pub fn content(plan_id: &str, targets: &[&str], now: i64) -> PlanContent {
        let job = manifest_of(&job_json(Some(("x", &ArtifactSha256::of(b"x")))));
        let typed = job.execute.typed().unwrap();
        let targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
        PlanContent {
            record_version: PLAN_RECORD_VERSION,
            plan_id: plan_id.into(),
            job_id: job.id.clone(),
            job_manifest_digest: job_manifest_digest(&job).unwrap(),
            role: typed.operation.expected_role(),
            operation: typed.operation.clone(),
            artifact: artifact(),
            targets: PlanContent::normalize_targets(&targets).unwrap(),
            not_before: now - 10,
            expires_at: now + 3600,
            execution_policy: ExecutionPolicy::SingleUsePerTarget,
        }
    }

    /// A correct create request (and the job it carries) for `plan_id`.
    pub fn request(plan_id: &str, targets: &[&str], now: i64) -> serde_json::Value {
        let c = content(plan_id, targets, now);
        let digest = c.digest();
        serde_json::json!({
            "plan_id": plan_id,
            "plan_digest": digest,
            "job": job_json(Some((plan_id, &digest))),
            "artifact": c.artifact,
            "targets": targets,
            "not_before": c.not_before,
            "expires_at": c.expires_at,
            "execution_policy": "single_use_per_target",
        })
    }

    pub fn create_request(v: serde_json::Value) -> CreatePlan {
        serde_json::from_value(v).unwrap()
    }

    /// Production plan routes + the production Exec feature gate.
    pub fn router(pool: &SqlitePool) -> Router {
        routes::<TestState>()
            .with_state(TestState {
                pool: pool.clone(),
                sink: AuditSink(None),
            })
            .layer(axum::middleware::from_fn(crate::auth::require_features))
    }

    pub fn claims(sub: &str, role: &str, features: Option<Vec<&str>>) -> crate::auth::Claims {
        crate::auth::Claims {
            sub: sub.into(),
            exp: 4_102_444_800,
            aud: None,
            roles: vec![role.into()],
            allowed_features: features.map(|f| {
                f.into_iter()
                    .map(|k| serde_json::from_value(serde_json::json!(k)).unwrap())
                    .collect()
            }),
        }
    }

    /// Issue one request as `claims` (`None` = unauthenticated).
    pub async fn call(
        router: &Router,
        claims: Option<crate::auth::Claims>,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(match body {
                Some(b) => Body::from(serde_json::to_vec(&b).unwrap()),
                None => Body::empty(),
            })
            .unwrap();
        if let Some(c) = claims {
            req.extensions_mut().insert(c);
        }
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, value)
    }

    pub fn operator(sub: &str) -> Option<crate::auth::Claims> {
        Some(claims(sub, "operator", None))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn create_ok(router: &Router, id: &str) -> serde_json::Value {
        let (s, v) = call(
            router,
            operator("alice"),
            "POST",
            "/api/deploy-plans",
            Some(request(
                id,
                &["PC-a", "pc-A"],
                chrono::Utc::now().timestamp(),
            )),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v
    }

    #[tokio::test]
    async fn authorized_lifecycle_over_the_real_router_and_database() {
        let pool = pool().await;
        let router = router(&pool);
        let created = create_ok(&router, "plan-1").await;
        assert_eq!(created["state"], "pending");
        assert_eq!(created["created_by"], "alice");
        assert_eq!(created["revision"], 0);
        assert_eq!(created["content"]["targets"], json!(["PC-a", "pc-A"]));
        let digest = created["plan_digest"].as_str().unwrap().to_owned();

        let (s, v) = call(
            &router,
            operator("bob"),
            "GET",
            "/api/deploy-plans/plan-1",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["plan_digest"], digest);
        let (s, v) = call(&router, operator("bob"), "GET", "/api/deploy-plans", None).await;
        assert_eq!((s, v.as_array().unwrap().len()), (StatusCode::OK, 1));

        // The approver is the authenticated account, not anything in the body.
        let (s, v) = call(
            &router,
            operator("bob"),
            "POST",
            "/api/deploy-plans/plan-1/approve",
            Some(json!({"plan_digest": digest, "revision": 0})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["state"], "approved");
        assert_eq!(v["approved_by"], "bob");
        assert_eq!(v["revision"], 1);

        // Replaying the approval, or approving a different revision, fails.
        let (s, _) = call(
            &router,
            operator("bob"),
            "POST",
            "/api/deploy-plans/plan-1/approve",
            Some(json!({"plan_digest": digest, "revision": 0})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);

        let (s, v) = call(
            &router,
            operator("carol"),
            "POST",
            "/api/deploy-plans/plan-1/revoke",
            Some(json!({"reason": "wrong window"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["state"], "revoked");
        assert_eq!(v["revoked_by"], "carol");
        assert_eq!(v["revoke_reason"], "wrong window");
        // Approval history survives revocation.
        assert_eq!(v["approved_by"], "bob");

        // A revoked plan cannot be approved again, nor revoked twice.
        let (s, _) = call(
            &router,
            operator("bob"),
            "POST",
            "/api/deploy-plans/plan-1/approve",
            Some(json!({"plan_digest": digest, "revision": 2})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _) = call(
            &router,
            operator("carol"),
            "POST",
            "/api/deploy-plans/plan-1/revoke",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);

        let (s, _) = call(
            &router,
            operator("bob"),
            "GET",
            "/api/deploy-plans/missing",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn identity_and_permission_come_from_the_authenticated_context() {
        let pool = pool().await;
        let router = router(&pool);
        create_ok(&router, "plan-1").await;
        let digest = load_record(&pool, "plan-1").await.unwrap().stored_digest;
        let approve_body = json!({"plan_digest": digest, "revision": 0});
        let create_body = request("plan-2", &["PC-a"], chrono::Utc::now().timestamp());

        let viewer = Some(claims("v", "viewer", None));
        let no_exec = Some(claims("o", "operator", Some(vec!["jobs"])));
        let with_exec = Some(claims("o", "operator", Some(vec!["exec"])));
        for (who, expect) in [
            (None, StatusCode::FORBIDDEN),
            (viewer, StatusCode::FORBIDDEN),
            (no_exec, StatusCode::FORBIDDEN),
        ] {
            for (method, uri, body) in [
                ("GET", "/api/deploy-plans", None),
                ("GET", "/api/deploy-plans/plan-1", None),
                ("POST", "/api/deploy-plans", Some(create_body.clone())),
                (
                    "POST",
                    "/api/deploy-plans/plan-1/approve",
                    Some(approve_body.clone()),
                ),
                ("POST", "/api/deploy-plans/plan-1/revoke", None),
            ] {
                let (s, _) = call(&router, who.clone(), method, uri, body).await;
                assert_eq!(
                    s,
                    expect,
                    "{method} {uri} as {:?}",
                    who.as_ref().map(|c| &c.roles)
                );
            }
        }
        // Nothing changed for the refused callers.
        let rec = load_record(&pool, "plan-1").await.unwrap();
        assert_eq!(rec.state, PlanState::Pending);
        assert!(load_record(&pool, "plan-2").await.is_err());
        // A feature-restricted operator who *does* hold exec may act.
        let (s, _) = call(
            &router,
            with_exec,
            "POST",
            "/api/deploy-plans/plan-1/approve",
            Some(approve_body.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // A caller-asserted approval or actor is rejected outright, and the
        // handler-level check also refuses a Caller with no operator standing
        // even if a route layer were missing.
        let mut asserted = request("plan-3", &["PC-a"], chrono::Utc::now().timestamp());
        asserted["approved"] = json!(true);
        let (s, _) = call(
            &router,
            operator("a"),
            "POST",
            "/api/deploy-plans",
            Some(asserted),
        )
        .await;
        assert!(s.is_client_error(), "{s}");
        let mut with_actor = approve_body;
        with_actor["approved_by"] = json!("admin");
        let (s, _) = call(
            &router,
            operator("a"),
            "POST",
            "/api/deploy-plans/plan-1/approve",
            Some(with_actor),
        )
        .await;
        assert!(s.is_client_error(), "{s}");
        assert!(actor(&Caller::default()).is_err());
        assert!(
            actor(&Caller {
                sub: Some("x".into()),
                operator: true,
                exec_feature: false,
                source: None
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn create_refuses_altered_or_inconsistent_content() {
        let pool = pool().await;
        let router = router(&pool);
        let now = chrono::Utc::now().timestamp();
        let post = |body: serde_json::Value| {
            let router = router.clone();
            async move {
                call(
                    &router,
                    operator("a"),
                    "POST",
                    "/api/deploy-plans",
                    Some(body),
                )
                .await
            }
        };

        let good = request("plan-ok", &["PC-a", "pc-A"], now);
        // Caller-supplied digest that is not the content's digest.
        let mut b = good.clone();
        b["plan_digest"] = json!(ArtifactSha256::of(b"forged"));
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        // Targets changed after the digest was computed (case differs).
        let mut b = good.clone();
        b["targets"] = json!(["pc-a", "pc-A"]);
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        // Duplicate targets are rejected, not silently de-duplicated.
        let mut b = request("plan-dup", &["PC-a"], now);
        b["targets"] = json!(["PC-a", "PC-a"]);
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        // Unregistered target, and a case-different spelling of a registered one.
        let (s, v) = post(request("plan-un", &["PC-a", "PC-A"], now)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
        assert!(v.as_str().unwrap().contains("PC-A"));
        // Artifact altered: the operation no longer matches the expected artifact.
        let mut b = good.clone();
        b["artifact"]["integrity"]["size_bytes"] = json!(22);
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        // The job names another plan / digest, or a different job body.
        let mut b = good.clone();
        b["job"]["deploy_authorization"]["plan_id"] = json!("other");
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        let mut b = good.clone();
        b["job"]["deploy_authorization"]["plan_digest"] = json!(ArtifactSha256::of(b"z"));
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        // A job edited after the digest was made changes the job digest.
        let mut b = good.clone();
        b["job"]["version"] = json!("1.0.1");
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        // Shell jobs are not plan subjects.
        let mut b = good.clone();
        b["job"] = json!({"id": "j", "version": "1.0.0",
            "execute": {"shell": "sh", "script": "x", "timeout": "30s"}});
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        // Unbounded / already-expired validity.
        let mut b = good.clone();
        b["expires_at"] = json!(now - 1);
        assert_eq!(post(b).await.0, StatusCode::BAD_REQUEST);
        let mut far = request("plan-far", &["PC-a"], now);
        far["expires_at"] = json!(now + MAX_PLAN_VALIDITY_SECS + 100);
        far["not_before"] = json!(now);
        assert_eq!(post(far).await.0, StatusCode::BAD_REQUEST);
        // Unknown field / unsupported policy.
        let mut b = good.clone();
        b["execution_policy"] = json!("repeat");
        assert!(post(b).await.0.is_client_error());
        // None of the above stored anything.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy_plans")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);

        // The good request works once; plan ids are never reused or edited.
        assert_eq!(post(good.clone()).await.0, StatusCode::CREATED);
        assert_eq!(post(good).await.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn approval_is_bound_to_the_reviewed_digest_revision_and_validity() {
        let pool = pool().await;
        let now = NOW;
        let req = create_request(request("plan-1", &["PC-a", "pc-A"], now));
        let view = create_plan(&pool, "alice", &req, now).await.unwrap();
        let digest = ArtifactSha256::try_from(view.plan_digest.clone()).unwrap();

        let wrong_digest = ApprovePlan {
            plan_digest: ArtifactSha256::of(b"other"),
            revision: 0,
        };
        let e = approve_plan(&pool, "plan-1", "bob", &wrong_digest, now)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        let wrong_rev = ApprovePlan {
            plan_digest: digest,
            revision: 7,
        };
        let e = approve_plan(&pool, "plan-1", "bob", &wrong_rev, now)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        let ok = ApprovePlan {
            plan_digest: digest,
            revision: 0,
        };
        // Expired plans cannot be approved.
        let e = approve_plan(&pool, "plan-1", "bob", &ok, now + 3600)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::GONE);
        let rec = load_record(&pool, "plan-1").await.unwrap();
        assert_eq!(rec.state, PlanState::Pending, "refusals changed nothing");
        let v = approve_plan(&pool, "plan-1", "bob", &ok, now)
            .await
            .unwrap();
        assert_eq!(v.state, PlanState::Approved);
        assert!(!v.expired);
        let later = approve_plan(&pool, "plan-1", "bob", &ok, now)
            .await
            .unwrap_err();
        assert_eq!(later.0, StatusCode::CONFLICT);
        // Read-side expiry is derived from the clock, not stored.
        let (rec, row) = load_full(&pool, "plan-1").await.unwrap();
        assert!(view_of(rec, row, now + 3600).expired);

        // Missing plan.
        let e = approve_plan(&pool, "nope", "bob", &ok, now)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_tampered_record_is_reported_never_trusted_or_approved() {
        let pool = pool().await;
        let req = create_request(request("plan-1", &["PC-a", "pc-A"], NOW));
        let view = create_plan(&pool, "alice", &req, NOW).await.unwrap();
        let digest = ArtifactSha256::try_from(view.plan_digest).unwrap();
        // Someone widens the stored targets without recomputing the digest.
        let content: String = sqlx::query_scalar("SELECT content FROM deploy_plans")
            .fetch_one(&pool)
            .await
            .unwrap();
        let widened = content.replace("\"PC-a\"", "\"PC-a\",\"PC-evil\"");
        assert_ne!(widened, content);
        sqlx::query("UPDATE deploy_plans SET content = ?")
            .bind(widened)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            load_record(&pool, "plan-1").await,
            Err(LoadError::Corrupt(_))
        ));
        let e = approve_plan(
            &pool,
            "plan-1",
            "bob",
            &ApprovePlan {
                plan_digest: digest,
                revision: 0,
            },
            NOW,
        )
        .await
        .unwrap_err();
        assert_eq!(e.0, StatusCode::INTERNAL_SERVER_ERROR);
        let state: String = sqlx::query_scalar("SELECT state FROM deploy_plans")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "pending");
    }

    #[tokio::test]
    async fn the_schema_makes_contradictory_approval_state_unrepresentable() {
        let pool = pool().await;
        let req = create_request(request("plan-1", &["PC-a", "pc-A"], NOW));
        create_plan(&pool, "alice", &req, NOW).await.unwrap();
        for bad in [
            // approved without an approver
            "UPDATE deploy_plans SET state = 'approved'",
            // approver without a timestamp
            "UPDATE deploy_plans SET approved_by = 'x'",
            // pending but carrying an approval
            "UPDATE deploy_plans SET approved_by = 'x', approved_at = 1",
            // revoked without a revoker
            "UPDATE deploy_plans SET state = 'revoked'",
            // approved while also revoked
            "UPDATE deploy_plans SET state = 'approved', approved_by = 'x', approved_at = 1, \
             revoked_by = 'y', revoked_at = 2",
            // unknown state
            "UPDATE deploy_plans SET state = 'applied'",
            // empty approver identity
            "UPDATE deploy_plans SET state = 'approved', approved_by = '', approved_at = 1",
            // agent plans do not exist
            "UPDATE deploy_plans SET role = 'agent'",
            // empty validity window
            "UPDATE deploy_plans SET expires_at = not_before",
        ] {
            assert!(
                sqlx::query(bad).execute(&pool).await.is_err(),
                "accepted: {bad}"
            );
        }
        // Coherent transitions are accepted.
        sqlx::query(
            "UPDATE deploy_plans SET state = 'approved', approved_by = 'x', approved_at = 1",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE deploy_plans SET state = 'revoked', revoked_by = 'y', revoked_at = 2")
            .execute(&pool)
            .await
            .unwrap();
    }

    /// File-backed database with several connections, so the races below are
    /// real contention on SQLite's write lock rather than a single connection.
    async fn shared_pool(dir: &tempfile::TempDir) -> SqlitePool {
        let path = dir.path().join("plans.db");
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true)
                    .busy_timeout(std::time::Duration::from_secs(30)),
            )
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        seed_agents(&pool).await;
        pool
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_approvals_have_exactly_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let pool = shared_pool(&dir).await;
        let req = create_request(request("plan-1", &["PC-a", "pc-A"], NOW));
        let view = create_plan(&pool, "alice", &req, NOW).await.unwrap();
        let digest = ArtifactSha256::try_from(view.plan_digest).unwrap();
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    approve_plan(
                        &pool,
                        "plan-1",
                        &format!("approver-{i}"),
                        &ApprovePlan {
                            plan_digest: digest,
                            revision: 0,
                        },
                        NOW,
                    )
                    .await
                })
            })
            .collect();
        let mut wins = 0;
        for t in tasks {
            match t.await.unwrap() {
                Ok(_) => wins += 1,
                Err(e) => assert_eq!(e.0, StatusCode::CONFLICT),
            }
        }
        assert_eq!(wins, 1);
        let rec = load_record(&pool, "plan-1").await.unwrap();
        assert_eq!(rec.state, PlanState::Approved);
        assert!(rec.approved_by.unwrap().starts_with("approver-"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_approve_and_revoke_never_leave_an_ambiguous_record() {
        for round in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            let pool = shared_pool(&dir).await;
            let id = format!("plan-{round}");
            let req = create_request(request(&id, &["PC-a", "pc-A"], NOW));
            let view = create_plan(&pool, "alice", &req, NOW).await.unwrap();
            let digest = ArtifactSha256::try_from(view.plan_digest).unwrap();
            let (p1, p2, id1, id2) = (pool.clone(), pool.clone(), id.clone(), id.clone());
            let approve = tokio::spawn(async move {
                approve_plan(
                    &p1,
                    &id1,
                    "bob",
                    &ApprovePlan {
                        plan_digest: digest,
                        revision: 0,
                    },
                    NOW,
                )
                .await
            });
            let revoke =
                tokio::spawn(async move { revoke_plan(&p2, &id2, "carol", None, NOW).await });
            let (a, r) = (approve.await.unwrap(), revoke.await.unwrap());
            // Revoke always lands (a pending or approved plan is revocable).
            assert!(r.is_ok(), "{r:?}");
            let rec = load_record(&pool, &id).await.unwrap();
            assert_eq!(rec.state, PlanState::Revoked, "revocation must be final");
            assert_eq!(rec.revoked_by.as_deref(), Some("carol"));
            // Approval either happened first (recorded) or lost cleanly.
            match a {
                Ok(_) => assert_eq!(rec.approved_by.as_deref(), Some("bob")),
                Err(e) => {
                    assert_eq!(e.0, StatusCode::CONFLICT);
                    assert_eq!(rec.approved_by, None);
                }
            }
        }
    }

    async fn approved_plan(pool: &SqlitePool, id: &str) -> (ArtifactSha256, i64) {
        let req = create_request(request(id, &["PC-a", "pc-A"], NOW));
        let view = create_plan(pool, "alice", &req, NOW).await.unwrap();
        let digest = ArtifactSha256::try_from(view.plan_digest).unwrap();
        let v = approve_plan(
            pool,
            id,
            "bob",
            &ApprovePlan {
                plan_digest: digest,
                revision: 0,
            },
            NOW,
        )
        .await
        .unwrap();
        (digest, v.revision)
    }

    #[tokio::test]
    async fn consume_is_single_use_and_rolls_back_atomically() {
        let pool = pool().await;
        let (digest, rev) = approved_plan(&pool, "plan-1").await;
        // Wrong digest / revision / outside the window cannot consume.
        assert_eq!(
            consume_plan(&pool, "plan-1", &ArtifactSha256::of(b"x"), rev, "e1", NOW).await,
            Err(ConsumeError::NotConsumable)
        );
        assert_eq!(
            consume_plan(&pool, "plan-1", &digest, rev + 1, "e1", NOW).await,
            Err(ConsumeError::NotConsumable)
        );
        assert_eq!(
            consume_plan(&pool, "plan-1", &digest, rev, "e1", NOW + 3600).await,
            Err(ConsumeError::NotConsumable)
        );
        assert_eq!(
            consume_plan(&pool, "plan-1", &digest, rev, "e1", NOW - 11).await,
            Err(ConsumeError::NotConsumable),
            "before not_before"
        );
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy_plan_applications")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "refusals recorded nothing");

        let targets = consume_plan(&pool, "plan-1", &digest, rev, "e1", NOW)
            .await
            .unwrap();
        assert_eq!(targets, vec!["PC-a".to_owned(), "pc-A".to_owned()]);
        // Replay with the fresh revision: the ledger refuses and nothing moves.
        let rev2: i64 = sqlx::query_scalar("SELECT revision FROM deploy_plans")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rev2, rev + 1);
        assert_eq!(
            consume_plan(&pool, "plan-1", &digest, rev2, "e2", NOW).await,
            Err(ConsumeError::Replay)
        );
        let rev3: i64 = sqlx::query_scalar("SELECT revision FROM deploy_plans")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rev3, rev2, "the failed replay rolled back");
        // The gate sees the ledger.
        assert_eq!(
            load_record(&pool, "plan-1")
                .await
                .unwrap()
                .applied_targets
                .len(),
            2
        );

        // A partially-applied ledger row (hand edit) makes consumption fail as a
        // whole instead of applying to the rest.
        let (digest, rev) = approved_plan(&pool, "plan-2").await;
        sqlx::query(
            "INSERT INTO deploy_plan_applications (plan_id, pc_id, exec_id, applied_at) \
             VALUES ('plan-2', 'pc-A', 'old', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            consume_plan(&pool, "plan-2", &digest, rev, "e3", NOW).await,
            Err(ConsumeError::Replay)
        );
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deploy_plan_applications WHERE plan_id = 'plan-2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "no partial application");
    }

    #[tokio::test]
    async fn pending_and_revoked_plans_cannot_be_consumed() {
        let pool = pool().await;
        let req = create_request(request("plan-1", &["PC-a", "pc-A"], NOW));
        let view = create_plan(&pool, "alice", &req, NOW).await.unwrap();
        let digest = ArtifactSha256::try_from(view.plan_digest).unwrap();
        assert_eq!(
            consume_plan(&pool, "plan-1", &digest, 0, "e", NOW).await,
            Err(ConsumeError::NotConsumable)
        );
        let (digest, rev) = approved_plan(&pool, "plan-2").await;
        revoke_plan(&pool, "plan-2", "carol", None, NOW)
            .await
            .unwrap();
        assert_eq!(
            consume_plan(&pool, "plan-2", &digest, rev + 1, "e", NOW).await,
            Err(ConsumeError::NotConsumable)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_consumption_and_revocation_have_one_consistent_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let pool = shared_pool(&dir).await;
        let (digest, rev) = approved_plan(&pool, "plan-1").await;
        let mut tasks = Vec::new();
        for i in 0..6 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                consume_plan(&pool, "plan-1", &digest, rev, &format!("e{i}"), NOW)
                    .await
                    .is_ok()
            }));
        }
        let revoke = {
            let pool = pool.clone();
            tokio::spawn(async move {
                revoke_plan(&pool, "plan-1", "carol", None, NOW)
                    .await
                    .is_ok()
            })
        };
        let mut consumed = 0;
        for t in tasks {
            consumed += usize::from(t.await.unwrap());
        }
        assert!(revoke.await.unwrap());
        assert!(
            consumed <= 1,
            "a plan is consumed at most once ({consumed})"
        );
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deploy_plan_applications")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 2 * consumed as i64, "ledger rows match the winners");
    }

    #[tokio::test]
    async fn deploy_plan_routes_map_to_the_exec_feature_only() {
        use crate::api::feature_for_path;
        use kanade_shared::feature::Feature;
        for p in [
            "/api/deploy-plans",
            "/api/deploy-plans/{plan_id}",
            "/api/deploy-plans/{plan_id}/approve",
            "/api/deploy-plans/{plan_id}/revoke",
        ] {
            assert_eq!(feature_for_path(p), Some(Feature::Exec), "{p}");
        }
    }
}
