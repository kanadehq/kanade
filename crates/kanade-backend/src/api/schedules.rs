use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::TryStreamExt;
use kanade_shared::kv::BUCKET_SCHEDULES;
use kanade_shared::manifest::Schedule;
use serde::Serialize;
use tracing::{info, warn};

use crate::api::AppState;
use crate::audit;

#[derive(Serialize)]
pub struct ScheduleSummary {
    pub id: String,
    pub cron: String,
    pub enabled: bool,
    pub job_id: String,
    pub job_version: String,
}

/// GET /api/schedules — full Schedule list, KV-backed.
pub async fn list(State(s): State<AppState>) -> Result<Json<Vec<Schedule>>, (StatusCode, String)> {
    let kv = match s.jetstream.get_key_value(BUCKET_SCHEDULES).await {
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
            && let Ok(sched) = serde_json::from_slice::<Schedule>(&bytes)
        {
            out.push(sched);
        }
    }
    Ok(Json(out))
}

/// POST /api/schedules — upsert.
pub async fn create(
    State(s): State<AppState>,
    Json(schedule): Json<Schedule>,
) -> Result<Json<ScheduleSummary>, (StatusCode, String)> {
    // Make sure the KV bucket exists (idempotent).
    let kv = s
        .jetstream
        .create_key_value(KvConfig {
            bucket: BUCKET_SCHEDULES.into(),
            history: 5,
            ..Default::default()
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ensure KV: {e}")))?;

    let body = serde_json::to_vec(&schedule)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
    kv.put(&schedule.id, body.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;
    info!(
        schedule_id = %schedule.id,
        cron = %schedule.cron,
        job_id = %schedule.manifest.id,
        "schedule upserted",
    );
    audit::record(
        &s.nats,
        "cli",
        "schedule_upsert",
        Some(&schedule.id),
        serde_json::json!({
            "cron": schedule.cron,
            "job_id": schedule.manifest.id,
            "enabled": schedule.enabled,
        }),
    )
    .await;
    Ok(Json(ScheduleSummary {
        id: schedule.id.clone(),
        cron: schedule.cron.clone(),
        enabled: schedule.enabled,
        job_id: schedule.manifest.id.clone(),
        job_version: schedule.manifest.version.clone(),
    }))
}

/// DELETE /api/schedules/{id}
pub async fn delete(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = match s.jetstream.get_key_value(BUCKET_SCHEDULES).await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "schedules KV missing on delete");
            return Err((StatusCode::NOT_FOUND, "schedules bucket missing".into()));
        }
    };
    kv.delete(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv delete: {e}")))?;
    info!(schedule_id = %id, "schedule deleted");
    audit::record(
        &s.nats,
        "cli",
        "schedule_delete",
        Some(&id),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
