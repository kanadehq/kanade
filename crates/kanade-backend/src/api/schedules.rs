use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use futures::TryStreamExt;
use kanade_shared::kv::{
    BUCKET_SCHEDULES, BUCKET_SCHEDULES_YAML, BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_REVOKED,
};
use kanade_shared::manifest::Schedule;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::api::AppState;
use crate::api::yaml_body::{YamlOrJson, mirror_yaml, yaml_headers};
use crate::audit;
use crate::audit::Caller;

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
///
/// Accepts JSON (`application/json`, default) or YAML
/// (`application/yaml`, `text/yaml`). YAML callers also populate the
/// parallel `BUCKET_SCHEDULES_YAML` so the SPA editor preserves
/// comments + formatting across edits. JSON callers fall back to a
/// `serde_yaml::to_string` mirror — best-effort, warn-logged on
/// failure.
pub async fn create(
    State(s): State<AppState>,
    caller: Caller,
    body: YamlOrJson<Schedule>,
) -> Result<Json<ScheduleSummary>, (StatusCode, String)> {
    let YamlOrJson {
        value: schedule,
        raw_yaml,
    } = body;
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

    let body_bytes = serde_json::to_vec(&schedule)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
    kv.put(&schedule.id, body_bytes.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;

    // Operator-facing YAML mirror — best-effort, same reasoning as
    // jobs::create. Scheduler/agent read the JSON catalog; the YAML
    // store only feeds the SPA editor.
    let yaml_source = raw_yaml.unwrap_or_else(|| {
        serde_yaml::to_string(&schedule)
            .unwrap_or_else(|_| String::from("# YAML mirror unavailable for this entry"))
    });
    if let Err(e) = mirror_yaml(&s, BUCKET_SCHEDULES_YAML, &schedule.id, &yaml_source).await {
        warn!(
            error = %e,
            schedule_id = %schedule.id,
            "schedules: YAML mirror put failed; JSON catalog is current",
        );
    }

    info!(
        schedule_id = %schedule.id,
        cron = %schedule.cron,
        job_id = %schedule.job_id,
        "schedule upserted",
    );
    audit::record(
        &s.nats,
        "operator",
        "schedule_upsert",
        Some(&schedule.id),
        Some(&caller),
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

/// `GET /api/schedules/{id}/yaml` — fetch the operator's YAML source
/// for the schedule. Falls back to a `serde_yaml::to_string` of the
/// JSON catalog row when the YAML mirror is missing (legacy entries
/// from before this endpoint).
pub async fn get_yaml(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, HeaderMap, String), (StatusCode, String)> {
    if let Ok(kv) = s.jetstream.get_key_value(BUCKET_SCHEDULES_YAML).await
        && let Ok(Some(bytes)) = kv.get(&id).await
        && let Ok(text) = String::from_utf8(bytes.to_vec())
    {
        return Ok((StatusCode::OK, yaml_headers(), text));
    }

    let kv = s
        .jetstream
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, format!("schedule '{id}' not found")))?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("schedule '{id}' not found")))?;
    let schedule: Schedule = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("decode: {e}")))?;
    let yaml = serde_yaml::to_string(&schedule).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode YAML: {e}"),
        )
    })?;
    Ok((StatusCode::OK, yaml_headers(), yaml))
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
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    let schedules_kv = s
        .jetstream
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .map_err(|e| {
            warn!(error = %e, "schedules KV missing on disable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("schedules bucket missing: {e}"),
            )
        })?;

    // Fetch the full Entry (not just the value) so we can use its
    // revision for an optimistic-concurrency `update` instead of a
    // blind `put`. Without that, a concurrent edit (operator changing
    // the cron expression while we're racing to disable) would be
    // silently clobbered — gemini #37 review flagged this as a
    // priority bug, and it lines up with the PR's "stop the rollout"
    // story where two operators reaching for the brake at once is a
    // realistic scenario.
    let entry = schedules_kv
        .entry(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV entry: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("schedule '{id}' not found")))?;
    let mut schedule: Schedule = serde_json::from_slice(&entry.value).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("deserialize stored schedule: {e}"),
        )
    })?;

    // Only write back if there's something to change. Skipping the
    // already-disabled case avoids a redundant watch event for the
    // backend / agent scheduler loops.
    if schedule.enabled {
        schedule.enabled = false;
        let body = serde_json::to_vec(&schedule).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialize schedule: {e}"),
            )
        })?;
        schedules_kv
            .update(&id, body.into(), entry.revision)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV update: {e}")))?;
    } else {
        info!(schedule_id = %id, "schedule already disabled; revoke-only path");
    }

    // Cascade Layer 2: revoke the underlying Manifest so already-
    // published Commands get caught at agent fire time. Same pattern
    // as `jobs::delete`: revoke is idempotent, status KV missing in
    // dev is a 503 so callers can `kanade jetstream setup` and retry.
    let cascade_applied = if q.cascade {
        let status_kv = s
            .jetstream
            .get_key_value(BUCKET_SCRIPT_STATUS)
            .await
            .map_err(|e| {
                warn!(
                    error = %e,
                    bucket = BUCKET_SCRIPT_STATUS,
                    "schedule_disable cascade: status KV unavailable",
                );
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("script_status bucket missing: {e}"),
                )
            })?;
        status_kv
            .put(&schedule.job_id, bytes::Bytes::from(SCRIPT_STATUS_REVOKED))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("script_status put: {e}"),
                )
            })?;
        info!(
            schedule_id = %id,
            job_id = %schedule.job_id,
            "schedule disabled with cascade revoke",
        );
        true
    } else {
        info!(schedule_id = %id, "schedule soft-disabled");
        false
    };

    audit::record(
        &s.nats,
        "operator",
        "schedule_disable",
        Some(&id),
        Some(&caller),
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
    caller: Caller,
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
        "operator",
        "schedule_delete",
        Some(&id),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
