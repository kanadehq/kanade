//! Cron-driven exec fan-out. Loads every enabled `Schedule` from the
//! `schedules` KV at startup *and* tails the bucket via `kv.watch_all()`
//! so future POST/DELETE through `/api/schedules` register and remove
//! jobs without bouncing the backend.
//!
//! Fires route through [`exec_manifest`] with actor = "scheduler", so
//! audit events split cleanly from operator-initiated `kanade exec`s.

pub mod policy;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use async_nats::jetstream::kv::Operation;
use chrono::{Duration as ChronoDuration, Local, Utc};
use futures::{StreamExt, TryStreamExt};
use kanade_shared::kv::{BUCKET_AGENT_GROUPS, BUCKET_SCHEDULES};
use kanade_shared::manifest::{ExecMode, FanoutPlan, RunsOn, Schedule, ScheduleTz, Target, When};
use sqlx::Row;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::api::exec::exec_manifest;
use policy::{Completion, FireAction, decide_fire};

/// `last_heartbeat` slack used to define "alive" for target
/// resolution. Matches the dashboard/health rollup cutoff so a
/// schedule's view of "all" lines up with what operators see in
/// the SPA.
const ALIVE_THRESHOLD: ChronoDuration = ChronoDuration::minutes(2);

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
    // v0.23: `runs_on: agent` schedules tick on the targeted
    // agents themselves; the backend's role is just to hold the
    // definition in the schedules KV so agents can read it. Skip
    // registration here.
    if matches!(schedule.runs_on, RunsOn::Agent) {
        info!(
            schedule_id = %schedule.id,
            "skipped (runs_on: agent — agents tick this schedule themselves)",
        );
        return Ok(());
    }

    // #418: operators write `when:`; the engine still runs on a
    // cron string — POLL_CRON (every minute) for reconcile shapes,
    // a 6/7-field cron for calendar shapes. #418 Phase 2: the cron
    // is evaluated in the schedule's tz via `new_async_tz`.
    let lowered = schedule.lowered();
    let cron = lowered.cron;
    let schedule_snapshot = schedule.clone();
    let cb = move |_uuid, _l| {
        let state = state.clone();
        let schedule = schedule_snapshot.clone();
        Box::pin(async move {
            tick(&state, schedule).await;
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    };
    let job = match lowered.tz {
        ScheduleTz::Utc => Job::new_async_tz(cron.as_str(), Utc, cb),
        ScheduleTz::Local => Job::new_async_tz(cron.as_str(), Local, cb),
    }
    .with_context(|| format!("Job::new_async_tz (cron={cron}, tz={:?})", lowered.tz))?;
    let uuid = sched.add(job).await.context("scheduler.add")?;
    registered.lock().await.insert(schedule.id.clone(), uuid);
    info!(
        schedule_id = %schedule.id,
        when = %schedule.when,
        poll_cron = %cron,
        tz = ?lowered.tz,
        "scheduled",
    );
    // A calendar one-shot whose date is already past lowers to a
    // year-stamped cron that never fires — surface that at register
    // time so "why didn't my one-shot run?" is diagnosable from the
    // log instead of silent (claude #432 review).
    if let When::Calendar(c) = &schedule.when {
        if let Some(fires_at) = c.oneshot_instant(schedule.tz) {
            if fires_at < Utc::now() {
                warn!(
                    schedule_id = %schedule.id,
                    %fires_at,
                    "calendar one-shot date is in the past — it will never fire",
                );
            }
        }
    }
    // A corrupt constraints.window fails closed (never fires) — warn
    // so the stuck schedule is diagnosable (gemini #452 review).
    if let Some(err) = schedule.bad_window() {
        warn!(
            schedule_id = %schedule.id,
            %err,
            "constraints.window is unparseable — schedule blocked (fail-closed) until fixed",
        );
    }
    // A calendar whose `at` time can never fall in its window also
    // never fires — warn instead of leaving a debug-only trail
    // (claude #452 review).
    if schedule.calendar_outside_window() {
        warn!(
            schedule_id = %schedule.id,
            when = %schedule.when,
            "calendar fire time is outside constraints.window — it will never fire",
        );
    }
    Ok(())
}

/// One cron-tick body: active-window gate → catalog lookup →
/// target resolution → policy decision → publish or skip.
async fn tick(state: &AppState, schedule: Schedule) {
    let schedule_id = schedule.id.clone();
    let job_id = schedule.job_id.clone();
    let lowered = schedule.lowered();

    // 0) Dormant outside the optional `active.{from,until}` window
    //    (#418 decision G). Cheapest check first — a finished
    //    campaign costs one comparison per tick, nothing else.
    if !schedule.active.contains(Utc::now(), schedule.tz) {
        tracing::debug!(%schedule_id, "scheduler tick: outside active window (dormant)");
        return;
    }

    // 0b) Maintenance window (#418 Phase 3): if a constraints.window
    //     is set, only fire when the current wall-clock time (in the
    //     schedule's tz) is inside it. Reconcile cadences resume the
    //     next minute the window reopens.
    if !schedule.constraints.allows(Utc::now(), schedule.tz) {
        tracing::debug!(%schedule_id, "scheduler tick: outside maintenance window — skip");
        return;
    }

    // 1) Resolve the registered Manifest at fire time so edits to
    //    the job catalog take effect on the next tick.
    let manifest = match crate::api::jobs::fetch(&state.jetstream, &job_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            warn!(
                %schedule_id, %job_id,
                "scheduler fire skipped: job not registered in catalog",
            );
            return;
        }
        Err(e) => {
            warn!(%schedule_id, %job_id, error = %e, "scheduler fire failed: catalog lookup error");
            return;
        }
    };

    // v0.22: stamp deadline_at = now + starting_deadline onto every
    // Command this tick emits. Agents that receive the Command after
    // this absolute time publish a synthetic skipped-result instead
    // of running the script. Use a helper so the parse-error path
    // logs once per tick, not once per Command.
    let now = Utc::now();
    let deadline_at = match parse_starting_deadline(schedule.starting_deadline.as_deref(), now) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                %schedule_id, error = %e,
                "scheduler fire failed: invalid starting_deadline",
            );
            return;
        }
    };
    let plan_for_dispatch = || {
        let mut p = schedule.plan.clone();
        p.deadline_at = deadline_at;
        p
    };

    // 2) For EveryTick (a calendar time trigger) we don't need to
    //    resolve anything — fire and forget. Skip the more expensive
    //    policy path entirely.
    if matches!(lowered.mode, ExecMode::EveryTick) {
        dispatch(
            state,
            &schedule_id,
            manifest,
            plan_for_dispatch(),
            "EveryTick",
        )
        .await;
        return;
    }

    // 3) Dedup modes need an expected-pc snapshot + recent
    //    completions for this manifest. Both are best-effort: an
    //    empty snapshot just means "skip this tick".
    let expected = match resolve_expected_pcs(state, &schedule.plan.target).await {
        Ok(v) => v,
        Err(e) => {
            warn!(%schedule_id, error = ?e, "scheduler fire failed: target resolve");
            return;
        }
    };
    let completions = match recent_completions(state, &job_id).await {
        Ok(v) => v,
        Err(e) => {
            warn!(%schedule_id, error = ?e, "scheduler fire failed: completion lookup");
            return;
        }
    };
    let cooldown = match parse_cooldown(lowered.cooldown.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            warn!(%schedule_id, error = %e, "scheduler fire failed: invalid when.every");
            return;
        }
    };

    let action = decide_fire(lowered.mode, cooldown, &expected, &completions, Utc::now());

    match action {
        FireAction::Skip => {
            tracing::debug!(
                %schedule_id, when = %schedule.when,
                expected = expected.len(),
                completions = completions.len(),
                "scheduler tick: dedup says skip",
            );
        }
        FireAction::FireWholeTarget => {
            dispatch(
                state,
                &schedule_id,
                manifest,
                plan_for_dispatch(),
                "OncePerTarget armed",
            )
            .await;
        }
        FireAction::FirePcs(pc_ids) => {
            let mut plan = plan_for_dispatch();
            // Per-pc dedup overrides the original target shape:
            // pcs only, drop rollout (rollout's group-wave model
            // doesn't compose with per-pc filtering).
            plan.target = Target {
                pcs: pc_ids.clone(),
                ..Target::default()
            };
            plan.rollout = None;
            info!(
                %schedule_id, pcs = pc_ids.len(),
                "OncePerPc: firing at remaining pcs",
            );
            dispatch(state, &schedule_id, manifest, plan, "OncePerPc subset").await;
        }
    }
}

async fn dispatch(
    state: &AppState,
    schedule_id: &str,
    manifest: kanade_shared::manifest::Manifest,
    plan: FanoutPlan,
    why: &str,
) {
    match exec_manifest(state, manifest, plan, "scheduler", None).await {
        Ok(resp) => info!(
            %schedule_id, exec_id = %resp.exec_id, why,
            "scheduler exec ok",
        ),
        Err((status, msg)) => warn!(
            %schedule_id, status = %status, error = %msg, why,
            "scheduler exec failed",
        ),
    }
}

fn parse_cooldown(s: Option<&str>) -> Result<Option<ChronoDuration>> {
    match s {
        None => Ok(None),
        Some(raw) => {
            let std: StdDuration = humantime::parse_duration(raw)
                .with_context(|| format!("parse cooldown '{raw}'"))?;
            Ok(Some(
                ChronoDuration::from_std(std).context("cooldown overflow")?,
            ))
        }
    }
}

/// Compute the absolute deadline this tick's Commands carry. Returns
/// `Ok(None)` when the schedule has no `starting_deadline` — meaning
/// the Command runs whenever delivered. Returns an error only when
/// the humantime string is malformed.
fn parse_starting_deadline(
    s: Option<&str>,
    now: chrono::DateTime<Utc>,
) -> Result<Option<chrono::DateTime<Utc>>> {
    match s {
        None => Ok(None),
        Some(raw) => {
            let std: StdDuration = humantime::parse_duration(raw)
                .with_context(|| format!("parse starting_deadline '{raw}'"))?;
            let d = ChronoDuration::from_std(std).context("starting_deadline overflow")?;
            Ok(Some(now + d))
        }
    }
}

/// Recent (exit_code = 0) completions for this job, one row per pc
/// (keeps `MAX(finished_at)` so the policy doesn't see stale
/// duplicates).
async fn recent_completions(state: &AppState, job_id: &str) -> Result<Vec<Completion>> {
    let rows = sqlx::query(
        "SELECT pc_id, MAX(finished_at) AS finished_at
         FROM execution_results
         WHERE job_id = ? AND exit_code = 0
         GROUP BY pc_id",
    )
    .bind(job_id)
    .fetch_all(&state.pool)
    .await
    .context("execution_results dedup query")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let pc_id: String = r.try_get("pc_id").unwrap_or_default();
        let finished_at: chrono::DateTime<Utc> = match r.try_get("finished_at") {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !pc_id.is_empty() {
            out.push(Completion { pc_id, finished_at });
        }
    }
    Ok(out)
}

/// Resolve the schedule's target to a concrete set of alive pc_ids
/// at tick time. "Alive" = `last_heartbeat` within
/// [`ALIVE_THRESHOLD`]; matches the dashboard's `active` rollup.
///
/// * `target.all`       → every alive agent
/// * `target.groups[*]` → alive agents in any listed group (KV scan)
/// * `target.pcs[*]`    → explicit list (no liveness filter — the
///   operator wrote the names themselves)
///
/// The three are unioned and deduped.
async fn resolve_expected_pcs(state: &AppState, target: &Target) -> Result<Vec<String>> {
    let mut out: HashSet<String> = HashSet::new();

    if target.all {
        let cutoff = Utc::now() - ALIVE_THRESHOLD;
        let rows = sqlx::query("SELECT pc_id FROM agents WHERE last_heartbeat >= ? ORDER BY pc_id")
            .bind(cutoff)
            .fetch_all(&state.pool)
            .await
            .context("agents alive query")?;
        for r in rows {
            if let Ok(pc) = r.try_get::<String, _>("pc_id") {
                out.insert(pc);
            }
        }
    }

    if !target.groups.is_empty() {
        // BUCKET_AGENT_GROUPS: key = pc_id, value = JSON list of group names.
        let want: HashSet<&str> = target.groups.iter().map(String::as_str).collect();
        let cutoff = Utc::now() - ALIVE_THRESHOLD;
        let alive: HashSet<String> =
            sqlx::query("SELECT pc_id FROM agents WHERE last_heartbeat >= ?")
                .bind(cutoff)
                .fetch_all(&state.pool)
                .await
                .context("alive list for group resolve")?
                .into_iter()
                .filter_map(|r| r.try_get::<String, _>("pc_id").ok())
                .collect();

        if let Ok(kv) = state.jetstream.get_key_value(BUCKET_AGENT_GROUPS).await {
            if let Ok(keys) = kv.keys().await {
                let keys: Vec<String> = keys.try_collect().await.unwrap_or_default();
                for k in keys {
                    if !alive.contains(&k) {
                        continue;
                    }
                    let Ok(Some(bytes)) = kv.get(&k).await else {
                        continue;
                    };
                    let Ok(groups) = serde_json::from_slice::<Vec<String>>(&bytes) else {
                        continue;
                    };
                    if groups.iter().any(|g| want.contains(g.as_str())) {
                        out.insert(k);
                    }
                }
            }
        }
    }

    for pc in &target.pcs {
        out.insert(pc.clone());
    }

    let mut v: Vec<String> = out.into_iter().collect();
    v.sort();
    Ok(v)
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
