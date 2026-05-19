use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use futures::TryStreamExt;
use kanade_shared::kv::{
    BUCKET_JOBS, BUCKET_SCHEDULES, BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_REVOKED,
};
use kanade_shared::manifest::{Manifest, Schedule};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::api::AppState;
use crate::audit;

#[derive(Serialize)]
pub struct ScheduleSummary {
    pub id: String,
    pub cron: String,
    pub enabled: bool,
    pub job_id: String,
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
        job_id = %schedule.job_id,
        "schedule upserted",
    );
    audit::record(
        &s.nats,
        "cli",
        "schedule_upsert",
        Some(&schedule.id),
        serde_json::json!({
            "cron": schedule.cron,
            "job_id": schedule.job_id,
            "enabled": schedule.enabled,
        }),
    )
    .await;
    Ok(Json(ScheduleSummary {
        id: schedule.id.clone(),
        cron: schedule.cron.clone(),
        enabled: schedule.enabled,
        job_id: schedule.job_id.clone(),
    }))
}

/// v0.27 — query params for [`disable`].
#[derive(Deserialize, Debug, Default)]
pub struct DisableQuery {
    /// When `true`, also Layer 2 cascade-revoke the underlying Job
    /// so any in-flight Command for `schedule.job_id` gets skipped
    /// by the agent's `handle_command` KV check. SPEC §2.6.4 (c)
    /// "hard disable". Default `false` = soft disable (cron stops,
    /// in-flight runs to completion).
    #[serde(default)]
    pub cascade: bool,
}

/// POST /api/schedules/{id}/disable
///
/// Two flavours, controlled by the `?cascade=` query param:
///
/// * **soft disable** (default, `cascade=false`): flip `enabled =
///   false` on the schedule in `BUCKET_SCHEDULES`. The cron loop
///   stops firing on the next watch tick (backend `scheduler.rs` +
///   agent `local_scheduler.rs` both watch this bucket). Already-
///   fired Commands are left alone — they run to completion or fail
///   on their own merits.
///
/// * **hard disable** (`cascade=true`): SPEC §2.6.4 (c). Soft disable
///   PLUS Layer 2 cascade — write
///   `script_status.{schedule.job_id} = REVOKED` so any Command
///   already in flight (live core sub delivery in progress or
///   sitting in `STREAM_EXEC` awaiting a reconnecting agent) gets
///   caught by the agent's Layer 2 KV check and skipped. The kill
///   cascade of *currently-running children* (Layer 3) is **not**
///   part of this PR — operators can follow up with
///   `kanade kill <job_id>` per execution. Tracked for v0.28 (needs
///   `executions.schedule_id` to find the in-flight job_ids the
///   schedule produced).
pub async fn disable(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DisableQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    let schedules_kv = match s.jetstream.get_key_value(BUCKET_SCHEDULES).await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "schedules KV missing on disable");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("schedules bucket missing: {e}"),
            ));
        }
    };

    let bytes = match schedules_kv.get(&id).await {
        Ok(Some(b)) => b,
        Ok(None) => return Err((StatusCode::NOT_FOUND, format!("schedule '{id}' not found"))),
        Err(e) => {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")));
        }
    };
    let mut schedule: Schedule = serde_json::from_slice(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("deserialize stored schedule: {e}"),
        )
    })?;

    // Soft step is always done first — revoke without a disabled
    // schedule would still let the cron fire fresh Commands that
    // would also be revoked, just noisier in the AUDIT log.
    if !schedule.enabled {
        info!(schedule_id = %id, "schedule already disabled; revoke-only path");
    }
    schedule.enabled = false;
    let body = serde_json::to_vec(&schedule).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize schedule: {e}"),
        )
    })?;
    schedules_kv
        .put(&id, body.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;

    // Cascade Layer 2: revoke the underlying Manifest so already-
    // published Commands get caught at agent fire time. Same pattern
    // as `jobs::delete`: revoke is idempotent, status KV missing in
    // dev is a 503 so callers can `kanade jetstream setup` and retry.
    let cascade_applied = if q.cascade {
        let status_kv = match s.jetstream.get_key_value(BUCKET_SCRIPT_STATUS).await {
            Ok(k) => k,
            Err(e) => {
                warn!(
                    error = %e,
                    bucket = BUCKET_SCRIPT_STATUS,
                    "schedule_disable cascade: status KV unavailable",
                );
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("script_status bucket missing: {e}"),
                ));
            }
        };
        status_kv
            .put(
                &schedule.job_id,
                bytes::Bytes::from(SCRIPT_STATUS_REVOKED.as_bytes()),
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("script_status put: {e}"),
                )
            })?;

        // Surface the Manifest existence in the audit payload so
        // operators don't get a misleading "cascade succeeded" when
        // the job_id was already a dangling reference.
        let manifest_exists = match s.jetstream.get_key_value(BUCKET_JOBS).await {
            Ok(jobs_kv) => match jobs_kv.get(&schedule.job_id).await {
                Ok(Some(bytes)) => serde_json::from_slice::<Manifest>(&bytes).is_ok(),
                _ => false,
            },
            Err(_) => false,
        };
        info!(
            schedule_id = %id,
            job_id = %schedule.job_id,
            manifest_exists,
            "schedule disabled with cascade revoke",
        );
        true
    } else {
        info!(schedule_id = %id, "schedule soft-disabled");
        false
    };

    audit::record(
        &s.nats,
        "cli",
        "schedule_disable",
        Some(&id),
        serde_json::json!({
            "cascade": cascade_applied,
            "job_id": schedule.job_id,
        }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
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
