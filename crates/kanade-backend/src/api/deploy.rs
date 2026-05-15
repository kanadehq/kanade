use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kanade_shared::kv::BUCKET_SCRIPT_CURRENT;
use kanade_shared::manifest::Manifest;
use kanade_shared::subject;
use kanade_shared::wire::Command;
use serde::Serialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;

#[derive(Serialize)]
pub struct DeployResponse {
    pub deploy_id: String,
    pub job_id: String,
    pub version: String,
    pub target_count: u32,
    pub subjects: Vec<String>,
}

pub async fn create(
    State(s): State<AppState>,
    Json(manifest): Json<Manifest>,
) -> Result<Json<DeployResponse>, (StatusCode, String)> {
    if !manifest.target.is_specified() {
        return Err((
            StatusCode::BAD_REQUEST,
            "target must specify at least one of `all` / `groups` / `pcs`".into(),
        ));
    }

    let timeout_secs = humantime::parse_duration(&manifest.execute.timeout)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid timeout: {e}")))?
        .as_secs();
    let jitter_secs = manifest
        .execute
        .jitter
        .as_deref()
        .map(humantime::parse_duration)
        .transpose()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid jitter: {e}")))?
        .map(|d| d.as_secs());

    let deploy_id = Uuid::new_v4().to_string();

    let mut subjects: Vec<String> = Vec::new();
    let mut target_count: u32 = 0;
    if manifest.target.all {
        subjects.push(subject::COMMANDS_ALL.to_string());
        target_count = target_count.saturating_add(1);
    }
    for g in &manifest.target.groups {
        subjects.push(subject::commands_group(g));
        target_count = target_count.saturating_add(1);
    }
    for pc in &manifest.target.pcs {
        subjects.push(subject::commands_pc(pc));
        target_count = target_count.saturating_add(1);
    }

    for subj in &subjects {
        let cmd = Command {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            request_id: Uuid::new_v4().to_string(),
            job_id: Some(deploy_id.clone()),
            shell: manifest.execute.shell.into(),
            script: manifest.execute.script.clone(),
            timeout_secs,
            jitter_secs,
        };
        let payload = serde_json::to_vec(&cmd)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
        if let Err(e) = s.nats.publish(subj.clone(), payload.into()).await {
            warn!(error = %e, subject = %subj, "publish failed");
            return Err((StatusCode::BAD_GATEWAY, format!("publish to {subj}: {e}")));
        }
    }
    let _ = s.nats.flush().await;

    // Spec §2.6 Layer 2 — pin the current version so stale Commands skip.
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
        "INSERT INTO deployments (deploy_id, job_id, version, initiated_by, target_count, status)
         VALUES (?, ?, ?, ?, ?, 'pending')",
    )
    .bind(&deploy_id)
    .bind(&manifest.id)
    .bind(&manifest.version)
    .bind("cli") // Sprint 3c: actual operator identity from auth
    .bind(target_count as i64)
    .execute(&s.pool)
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert deployments: {e}"),
        )
    })?;

    info!(
        deploy_id = %deploy_id,
        job_id = %manifest.id,
        version = %manifest.version,
        target_count,
        subjects = ?subjects,
        "deployment published",
    );

    Ok(Json(DeployResponse {
        deploy_id,
        job_id: manifest.id,
        version: manifest.version,
        target_count,
        subjects,
    }))
}
