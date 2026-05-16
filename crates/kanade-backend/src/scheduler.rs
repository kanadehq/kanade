//! Cron-driven deploy fan-out. Loads every enabled `Schedule` from the
//! `schedules` KV at startup *and* tails the bucket via `kv.watch_all()`
//! so future POST/DELETE through `/api/schedules` register and remove
//! jobs without bouncing the backend.
//!
//! Fires route through [`deploy_manifest`] with actor = "scheduler", so
//! audit events split cleanly from operator-initiated `kanade deploy`s.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_nats::jetstream::kv::Operation;
use futures::{StreamExt, TryStreamExt};
use kanade_shared::kv::BUCKET_SCHEDULES;
use kanade_shared::manifest::Schedule;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::api::deploy::deploy_manifest;

type Registered = Arc<Mutex<HashMap<String, Uuid>>>;

pub async fn run(state: AppState) -> Result<()> {
    // Always create-or-attach to the schedules KV at boot so the watch
    // loop is live for the first `kanade schedule create` even on a
    // fresh broker (otherwise the get-only path would idle until a
    // setup-time KV provisioning step ran).
    let kv = state
        .jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: BUCKET_SCHEDULES.into(),
            history: 5,
            ..Default::default()
        })
        .await
        .context("ensure schedules KV")?;

    let sched = JobScheduler::new().await.context("init JobScheduler")?;
    sched.start().await.context("start JobScheduler")?;
    let registered: Registered = Arc::new(Mutex::new(HashMap::new()));

    // 1. Initial load — register every enabled Schedule already in KV.
    //
    // Best-effort: kv.keys() against an empty bucket fails on
    // async-nats 0.48 (the internal LastPerSubject ordered-consumer
    // returns an error when the stream has zero messages). Failing
    // the whole scheduler over that would take down the watch loop
    // too — which is exactly the bit that catches the first
    // schedule POST after a fresh broker boot. Log + continue so
    // the watch loop stays live; the initial set just stays empty
    // until the first real schedule lands.
    let keys: Vec<String> = match kv.keys().await {
        Ok(stream) => stream.try_collect().await.unwrap_or_else(|e| {
            warn!(error = %e, "collect schedules KV keys (initial load best-effort)");
            Vec::new()
        }),
        Err(e) => {
            warn!(error = %e, "list schedules KV keys (likely empty bucket; watch loop still arms)");
            Vec::new()
        }
    };
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
            Ok(s) if s.enabled => {
                if let Err(e) = register(&sched, state.clone(), &registered, s.clone()).await {
                    warn!(error = %e, schedule_id = %s.id, "initial register failed");
                }
            }
            Ok(s) => info!(schedule_id = %s.id, "skipped (disabled)"),
            Err(e) => warn!(error = %e, key = %k, "deserialize Schedule"),
        }
    }
    // Snapshot the count before any subsequent await so the MutexGuard
    // doesn't live across the watch loop (Send bound for tokio::spawn).
    let initial_count = registered.lock().await.len();
    info!(
        count = initial_count,
        "scheduler registered initial schedules"
    );

    // 2. Watch — react to KV puts/deletes for the lifetime of the process.
    let mut watcher = kv.watch_all().await.context("kv watch_all")?;
    while let Some(entry) = watcher.next().await {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "watch entry error");
                continue;
            }
        };
        match entry.operation {
            Operation::Put => {
                let sched_data: Schedule = match serde_json::from_slice(&entry.value) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, key = %entry.key, "deserialize Schedule on watch");
                        continue;
                    }
                };
                // Replace any existing registration so cron/manifest edits stick.
                unregister(&sched, &registered, &sched_data.id).await;
                if sched_data.enabled
                    && let Err(e) =
                        register(&sched, state.clone(), &registered, sched_data.clone()).await
                {
                    warn!(error = %e, schedule_id = %sched_data.id, "watch register failed");
                }
            }
            Operation::Delete | Operation::Purge => {
                unregister(&sched, &registered, &entry.key).await;
            }
        }
    }

    // watch_all is theoretically infinite; if it ever yields None keep the
    // scheduler alive anyway so existing jobs keep firing.
    std::future::pending::<Result<()>>().await
}

async fn register(
    sched: &JobScheduler,
    state: AppState,
    registered: &Registered,
    schedule: Schedule,
) -> Result<()> {
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
    let uuid = sched.add(job).await.context("scheduler.add")?;
    registered.lock().await.insert(schedule.id.clone(), uuid);
    info!(schedule_id = %schedule.id, cron = %schedule.cron, "scheduled");
    Ok(())
}

async fn unregister(sched: &JobScheduler, registered: &Registered, schedule_id: &str) {
    let removed = registered.lock().await.remove(schedule_id);
    if let Some(uuid) = removed {
        if let Err(e) = sched.remove(&uuid).await {
            warn!(error = %e, schedule_id, "scheduler.remove failed");
        } else {
            info!(schedule_id, "scheduler unregistered");
        }
    }
}
