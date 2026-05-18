//! Job endpoints:
//!   * `POST /api/jobs/{job_id}/kill` — runtime control. Publishes on
//!     `kill.{job_id}` so any agent currently executing a Command with
//!     that job_id terminates its child process (spec §2.6 Layer 3).
//!   * `GET / POST /api/jobs` + `DELETE /api/jobs/{id}` — catalog CRUD
//!     (v0.15). Schedules reference catalog rows by `job_id`.

use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::TryStreamExt;
use kanade_shared::kv::{BUCKET_JOBS, BUCKET_SCHEDULES};
use kanade_shared::manifest::{Manifest, Schedule};
use kanade_shared::subject;
use serde::Serialize;
use tracing::{info, warn};

use super::AppState;
use crate::audit;

pub async fn kill(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .nats
        .publish(subject::kill(&job_id), bytes::Bytes::new())
        .await
        .map_err(|e| {
            warn!(error = %e, job_id, "publish kill");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("publish kill.{job_id}: {e}"),
            )
        })?;
    // flush so the subject is on the wire before we ack the
    // operator — without it, a fast operator-then-shutdown could
    // theoretically drop the kill on the floor.
    let _ = state.nats.flush().await;
    info!(job_id = %job_id, "kill signal published");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct JobSummary {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
    pub inventory: bool,
}

/// GET /api/jobs — list every registered job.
pub async fn list(State(s): State<AppState>) -> Result<Json<Vec<Manifest>>, (StatusCode, String)> {
    let kv = match s.jetstream.get_key_value(BUCKET_JOBS).await {
        Ok(k) => k,
        Err(_) => return Ok(Json(Vec::new())),
    };
    let keys_stream = kv
        .keys()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?;
    let keys: Vec<String> = keys_stream
        .try_collect()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?;
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        if let Ok(Some(bytes)) = kv.get(&k).await
            && let Ok(job) = serde_json::from_slice::<Manifest>(&bytes)
        {
            out.push(job);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(out))
}

/// POST /api/jobs — upsert a Manifest into the job catalog. The KV
/// key is `manifest.id`.
pub async fn create(
    State(s): State<AppState>,
    Json(job): Json<Manifest>,
) -> Result<Json<JobSummary>, (StatusCode, String)> {
    let kv = s
        .jetstream
        .create_key_value(KvConfig {
            bucket: BUCKET_JOBS.into(),
            history: 5,
            ..Default::default()
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ensure KV: {e}")))?;
    let body = serde_json::to_vec(&job)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
    kv.put(&job.id, body.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;
    let summary = JobSummary {
        id: job.id.clone(),
        version: job.version.clone(),
        description: job.description.clone(),
        inventory: job.inventory.is_some(),
    };
    info!(job_id = %job.id, version = %job.version, "job upserted");
    audit::record(
        &s.nats,
        "cli",
        "job_upsert",
        Some(&job.id),
        serde_json::json!({
            "version": job.version,
            "inventory": job.inventory.is_some(),
        }),
    )
    .await;
    Ok(Json(summary))
}

/// DELETE /api/jobs/{id} — 409 if any Schedule references it.
pub async fn delete(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    if let Ok(kv) = s.jetstream.get_key_value(BUCKET_SCHEDULES).await
        && let Ok(keys_stream) = kv.keys().await
    {
        let keys: Vec<String> = keys_stream.try_collect().await.unwrap_or_default();
        for k in keys {
            if let Ok(Some(bytes)) = kv.get(&k).await
                && let Ok(sched) = serde_json::from_slice::<Schedule>(&bytes)
                && sched.job_id == id
            {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "job '{id}' is referenced by schedule '{}'; remove the schedule first",
                        sched.id
                    ),
                ));
            }
        }
    }

    let kv = match s.jetstream.get_key_value(BUCKET_JOBS).await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "jobs KV missing on delete");
            return Err((StatusCode::NOT_FOUND, "jobs bucket missing".into()));
        }
    };
    kv.delete(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv delete: {e}")))?;
    info!(job_id = %id, "job deleted");
    audit::record(
        &s.nats,
        "cli",
        "job_delete",
        Some(&id),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Lookup helper for scheduler + projector. Returns `Ok(None)` when
/// the key is absent so callers can warn-and-skip without unwrapping
/// a fatal error.
pub async fn fetch(
    js: &async_nats::jetstream::Context,
    job_id: &str,
) -> anyhow::Result<Option<Manifest>> {
    let kv = match js.get_key_value(BUCKET_JOBS).await {
        Ok(k) => k,
        Err(_) => return Ok(None),
    };
    let Some(bytes) = kv.get(job_id).await? else {
        return Ok(None);
    };
    let job: Manifest = serde_json::from_slice(&bytes)?;
    Ok(Some(job))
}
