//! Cron-driven deploy fan-out. Reads the `schedules` KV bucket once at
//! startup and registers each enabled `Schedule` with
//! `tokio_cron_scheduler::JobScheduler`. When a job fires it routes
//! through the same [`deploy_manifest`] helper the HTTP handler uses,
//! tagged with `actor = "scheduler"` so audit events can be split.
//!
//! Sprint 3c.scheduler limitation: dynamic re-registration on KV updates
//! is not wired yet — bouncing the backend picks up new schedules.

use anyhow::{Context, Result};
use futures::TryStreamExt;
use kanade_shared::kv::BUCKET_SCHEDULES;
use kanade_shared::manifest::Schedule;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{info, warn};

use crate::api::AppState;
use crate::api::deploy::deploy_manifest;

pub async fn run(state: AppState) -> Result<()> {
    let kv = match state.jetstream.get_key_value(BUCKET_SCHEDULES).await {
        Ok(k) => k,
        Err(_) => {
            info!(
                bucket = BUCKET_SCHEDULES,
                "schedules KV missing — scheduler idle (POST a schedule to create the bucket)"
            );
            return std::future::pending::<Result<()>>().await;
        }
    };

    let sched = JobScheduler::new().await.context("init JobScheduler")?;
    sched.start().await.context("start JobScheduler")?;

    let keys_stream = kv.keys().await.context("list schedules KV keys")?;
    let keys: Vec<String> = keys_stream.try_collect().await.context("collect KV keys")?;
    let mut count: u32 = 0;
    for k in keys {
        let entry = match kv.get(&k).await {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, key = %k, "kv get");
                continue;
            }
        };
        match serde_json::from_slice::<Schedule>(&entry) {
            Ok(s) => {
                if !s.enabled {
                    info!(schedule_id = %s.id, "skipped (disabled)");
                    continue;
                }
                match register(&sched, state.clone(), s.clone()).await {
                    Ok(()) => count += 1,
                    Err(e) => {
                        warn!(error = %e, schedule_id = %s.id, "register failed")
                    }
                }
            }
            Err(e) => warn!(error = %e, key = %k, "deserialize Schedule"),
        }
    }
    info!(count, "scheduler registered initial schedules");

    // Keep the JobScheduler alive for the rest of the process. Without
    // this the scheduler instance would drop and stop firing.
    std::future::pending::<Result<()>>().await
}

async fn register(sched: &JobScheduler, state: AppState, schedule: Schedule) -> Result<()> {
    let cron = schedule.cron.clone();
    let schedule_id = schedule.id.clone();
    let manifest = schedule.manifest.clone();
    let job = Job::new_async(cron.as_str(), move |_uuid, _l| {
        let state = state.clone();
        let manifest = manifest.clone();
        let schedule_id = schedule_id.clone();
        Box::pin(async move {
            info!(
                schedule_id = %schedule_id,
                job_id = %manifest.id,
                "scheduler firing",
            );
            match deploy_manifest(&state, manifest, "scheduler").await {
                Ok(resp) => info!(
                    schedule_id = %schedule_id,
                    deploy_id = %resp.deploy_id,
                    "scheduler deploy ok",
                ),
                Err((status, msg)) => warn!(
                    schedule_id = %schedule_id,
                    status = %status,
                    error = %msg,
                    "scheduler deploy failed",
                ),
            }
        })
    })
    .with_context(|| format!("Job::new_async (cron={cron})"))?;
    sched.add(job).await.context("scheduler.add")?;
    info!(schedule_id = %schedule.id, cron = %schedule.cron, "scheduled");
    Ok(())
}
