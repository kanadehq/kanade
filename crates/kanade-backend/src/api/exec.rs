use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use kanade_shared::kv::BUCKET_SCRIPT_CURRENT;
use kanade_shared::manifest::{FanoutPlan, Manifest};
use kanade_shared::subject;
use kanade_shared::wire::Command;
use serde::Serialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::api::jobs;
use crate::audit;
use crate::audit::Caller;

#[derive(Serialize, Clone)]
pub struct ExecResponse {
    pub exec_id: String,
    pub job_id: String,
    pub version: String,
    pub target_count: u32,
    pub subjects: Vec<String>,
}

/// Core exec pipeline used by both the HTTP handler (actor = "operator")
/// and the scheduler (actor = "scheduler"). Validates the [`FanoutPlan`],
/// fans the Command out across every target subject (or schedules
/// wave-based fan-out when `rollout` is set), pins script_current,
/// records an `executions` row, and emits an audit event.
///
/// v0.18: the Manifest supplies only "what to run" (script + shell +
/// timeout + inventory hint). `target` / `rollout` / `jitter` come
/// from the caller's [`FanoutPlan`] — schedules carry one inline,
/// ad-hoc execs build one from CLI flags / SPA form input.
///
/// `caller` is `None` for the scheduler path (no HTTP request) and
/// `Some(_)` for operator-initiated execs, where the JWT subject and
/// `X-Kanade-Source` header get folded into the audit payload.
pub async fn exec_manifest(
    s: &AppState,
    manifest: Manifest,
    plan: FanoutPlan,
    actor: &str,
    caller: Option<&Caller>,
) -> Result<ExecResponse, (StatusCode, String)> {
    let has_rollout = plan
        .rollout
        .as_ref()
        .map(|r| !r.waves.is_empty())
        .unwrap_or(false);
    if !has_rollout && !plan.target.is_specified() {
        return Err((
            StatusCode::BAD_REQUEST,
            "target must specify at least one of `all` / `groups` / `pcs` (or set `rollout.waves`)"
                .into(),
        ));
    }

    let timeout_secs = humantime::parse_duration(&manifest.execute.timeout)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid timeout: {e}")))?
        .as_secs();
    let jitter_secs = plan
        .jitter
        .as_deref()
        .map(humantime::parse_duration)
        .transpose()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid jitter: {e}")))?
        .map(|d| d.as_secs());

    let exec_id = Uuid::new_v4().to_string();

    let deadline_at = plan.deadline_at;
    let make_cmd = || Command {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        request_id: Uuid::new_v4().to_string(),
        job_id: Some(exec_id.clone()),
        shell: manifest.execute.shell.into(),
        script: manifest.execute.script.clone(),
        timeout_secs,
        jitter_secs,
        run_as: manifest.execute.run_as,
        cwd: manifest.execute.cwd.clone(),
        deadline_at,
        // v0.26: forward Layer 2 staleness policy to every emitted
        // Command so the agent can apply it whether it sees the live
        // publish or replays from STREAM_EXEC on reconnect.
        staleness: manifest.staleness.clone(),
    };

    let mut subjects: Vec<String> = Vec::new();
    let mut target_count: u32 = 0;

    if let Some(rollout) = plan.rollout.as_ref() {
        // Wave-based fan-out: pre-validate every delay so a bad humantime
        // string aborts the whole exec instead of silently failing on a
        // late wave inside a tokio::spawn.
        let mut delays = Vec::with_capacity(rollout.waves.len());
        for (idx, wave) in rollout.waves.iter().enumerate() {
            let d = humantime::parse_duration(&wave.delay).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("invalid rollout.waves[{idx}].delay: {e}"),
                )
            })?;
            delays.push(d);
        }

        for ((idx, wave), delay) in rollout.waves.iter().enumerate().zip(delays) {
            let subj = subject::commands_group(&wave.group);
            subjects.push(subj.clone());
            target_count = target_count.saturating_add(1);

            let cmd = make_cmd();
            let payload = serde_json::to_vec(&cmd)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;

            if delay.is_zero() {
                if let Err(e) = s.nats.publish(subj.clone(), payload.into()).await {
                    return Err((StatusCode::BAD_GATEWAY, format!("publish to {subj}: {e}")));
                }
                info!(wave = idx, subject = %subj, "wave published (immediate)");
            } else {
                let nats = s.nats.clone();
                let subj_for_spawn = subj.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    match nats.publish(subj_for_spawn.clone(), payload.into()).await {
                        Ok(()) => info!(
                            wave = idx,
                            subject = %subj_for_spawn,
                            delay_secs = delay.as_secs(),
                            "wave published (delayed)",
                        ),
                        Err(e) => warn!(
                            error = %e,
                            wave = idx,
                            subject = %subj_for_spawn,
                            "delayed wave publish failed",
                        ),
                    }
                });
                info!(
                    wave = idx,
                    subject = %subj,
                    delay_secs = delay.as_secs(),
                    "wave scheduled",
                );
            }
        }
    } else {
        // Plain target-based fan-out (no rollout block).
        if plan.target.all {
            subjects.push(subject::COMMANDS_ALL.to_string());
            target_count = target_count.saturating_add(1);
        }
        for g in &plan.target.groups {
            subjects.push(subject::commands_group(g));
            target_count = target_count.saturating_add(1);
        }
        for pc in &plan.target.pcs {
            subjects.push(subject::commands_pc(pc));
            target_count = target_count.saturating_add(1);
        }

        for subj in &subjects {
            let cmd = make_cmd();
            let payload = serde_json::to_vec(&cmd)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
            if let Err(e) = s.nats.publish(subj.clone(), payload.into()).await {
                warn!(error = %e, subject = %subj, "publish failed");
                return Err((StatusCode::BAD_GATEWAY, format!("publish to {subj}: {e}")));
            }
        }
    }
    let _ = s.nats.flush().await;

    match s.jetstream.get_key_value(BUCKET_SCRIPT_CURRENT).await {
        Ok(kv) => {
            if let Err(e) = kv
                .put(
                    &manifest.id,
                    bytes::Bytes::from(manifest.version.clone().into_bytes()),
                )
                .await
            {
                warn!(error = %e, cmd_id = %manifest.id, "script_current put failed");
            }
        }
        Err(e) => warn!(error = %e, "script_current KV missing; skipping version pin"),
    }

    sqlx::query(
        "INSERT INTO executions (exec_id, job_id, version, initiated_by, target_count, status)
         VALUES (?, ?, ?, ?, ?, 'pending')",
    )
    .bind(&exec_id)
    .bind(&manifest.id)
    .bind(&manifest.version)
    .bind(actor)
    .bind(target_count as i64)
    .execute(&s.pool)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert executions: {e}"),
        )
    })?;

    info!(
        exec_id = %exec_id,
        job_id = %manifest.id,
        version = %manifest.version,
        actor,
        target_count,
        wave_mode = has_rollout,
        subjects = ?subjects,
        "execution published",
    );

    audit::record(
        &s.nats,
        actor,
        "exec",
        Some(&manifest.id),
        caller,
        serde_json::json!({
            "exec_id": exec_id,
            "version": manifest.version,
            "target_count": target_count,
            "subjects": subjects,
            "wave_mode": has_rollout,
        }),
    )
    .await;

    Ok(ExecResponse {
        exec_id,
        job_id: manifest.id,
        version: manifest.version,
        target_count,
        subjects,
    })
}

/// `POST /api/exec/{job_id}` — fire a registered job from the
/// catalog against a caller-supplied [`FanoutPlan`]. The Manifest body
/// is never accepted inline; operators must `kanade job create` first.
/// 404 when the job isn't in `BUCKET_JOBS`.
pub async fn create(
    State(s): State<AppState>,
    Path(job_id): Path<String>,
    caller: Caller,
    Json(plan): Json<FanoutPlan>,
) -> Result<Json<ExecResponse>, (StatusCode, String)> {
    let manifest = match jobs::fetch(&s.jetstream, &job_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                format!(
                    "job '{job_id}' not found in catalog — register it first with \
                     `kanade job create <manifest.yaml>`"
                ),
            ));
        }
        Err(e) => {
            warn!(error = %e, %job_id, "exec: job catalog lookup failed");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("job catalog lookup: {e}"),
            ));
        }
    };
    exec_manifest(&s, manifest, plan, "operator", Some(&caller))
        .await
        .map(Json)
}
