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
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use futures::{StreamExt, TryStreamExt};
use kanade_shared::kv::{
    BUCKET_AGENT_GROUPS, BUCKET_FLEET_CONFIG, BUCKET_SCHEDULER_DISPATCH, BUCKET_SCHEDULES,
    KEY_FREEZE, dispatch_mark_pc_key, dispatch_mark_target_key,
};
use kanade_shared::manifest::{
    ExecMode, FanoutPlan, Freeze, Manifest, RunsOn, Schedule, ScheduleTz, Target, When,
};
use kanade_shared::wire::AgentGroups;
use sqlx::Row;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::api::exec::exec_manifest;
use policy::{Completion, FireAction, decide_fire, suppress_dispatched};

/// `last_heartbeat` slack used to define "alive" for target
/// resolution. Matches the dashboard/health rollup cutoff so a
/// schedule's view of "all" lines up with what operators see in
/// the SPA.
const ALIVE_THRESHOLD: ChronoDuration = ChronoDuration::minutes(2);

/// Drain margin folded into the in-flight suppression window — slack
/// for a finished run's `ExecResult` to travel agent outbox → backend
/// projector → `execution_results` after the script itself returns.
const DISPATCH_DRAIN_MARGIN: ChronoDuration = ChronoDuration::seconds(90);
/// Floor for the suppression window so a zero-jitter, sub-second job
/// still gets one poll tick of cover before it can re-fire.
const DISPATCH_WINDOW_MIN: ChronoDuration = ChronoDuration::seconds(90);
/// Ceiling for the suppression window. Past this the completion-based
/// dedup is the backstop, so an outsized jitter/timeout can't mute a
/// schedule indefinitely on the strength of a dispatch that may have
/// gone nowhere.
const DISPATCH_WINDOW_MAX: ChronoDuration = ChronoDuration::minutes(30);
/// Bucket-wide TTL on dispatch marks. Comfortably larger than
/// [`DISPATCH_WINDOW_MAX`] so a mark is never GC'd while still inside
/// its suppression window, but small enough that the bucket self-trims.
const DISPATCH_MARK_TTL: StdDuration = StdDuration::from_secs(60 * 60);
/// Concurrency cap for the per-pc dispatch-mark KV reads/writes. A
/// `target: all` OncePerPc fire can touch the whole fleet's worth of
/// pcs; doing those NATS round-trips serially would stall the tick, so
/// they run `buffer_unordered` up to this many at once (gemini #444).
const DISPATCH_KV_CONCURRENCY: usize = 16;

type Registered = Arc<Mutex<HashMap<String, Uuid>>>;

/// In-memory mirror of the fleet change-freeze (#418 Phase 5), kept
/// fresh by [`spawn_freeze_watcher`]. `tick` reads this instead of a
/// per-fire KV round-trip — the watch keeps it current without making
/// every cron tick block on a `get_key_value` (gemini #472).
type FreezeMirror = Arc<tokio::sync::RwLock<Option<Freeze>>>;

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

    // In-flight dispatch marks for bounded re-fire suppression. Best
    // effort: if the bucket can't be provisioned, suppression simply
    // degrades to the pre-existing completion-only dedup (which over-
    // fires during the jitter/drain gap but is never wrong), so a KV
    // hiccup must not take the scheduler down.
    if let Err(e) = state
        .jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: BUCKET_SCHEDULER_DISPATCH.into(),
            history: 1,
            max_age: DISPATCH_MARK_TTL,
            ..Default::default()
        })
        .await
    {
        // `create_key_value` only errors when the bucket already exists
        // with a *different* config (matching config is idempotent), or
        // on a genuine provisioning failure. The former is benign — e.g.
        // a future DISPATCH_MARK_TTL bump on an existing bucket: the
        // bucket is still there, so the per-tick get/put below keep
        // working (with the prior config) and suppression is unaffected.
        // Only a true failure leaves the bucket absent, in which case the
        // get_key_value calls fail open to the completion-only dedup.
        // Either way, never take the scheduler down — just note it.
        warn!(error = %e, "ensure scheduler_dispatch KV failed (benign if the bucket already exists with a prior config; a genuine failure falls back to completion-only dedup)");
    }

    let sched = JobScheduler::new().await.context("init JobScheduler")?;
    sched.start().await.context("start JobScheduler")?;
    let registered: Registered = Arc::new(Mutex::new(HashMap::new()));

    // #418 Phase 5: seed the fleet change-freeze mirror once, then keep
    // it fresh with a watch task. Every `tick` reads this Arc — no
    // per-fire KV get (gemini #472). The seed is synchronous so the
    // first ticks already see a freeze set before this boot. A seed-
    // read failure is logged (NOT silently treated as "not frozen" —
    // coderabbit #472); the watch re-seeds from its initial delivery.
    let initial_freeze = match load_freeze(&state.jetstream).await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "freeze boot-seed read failed; mirror starts empty, watch will seed on connect");
            None
        }
    };
    let freeze: FreezeMirror = Arc::new(tokio::sync::RwLock::new(initial_freeze));
    spawn_freeze_watcher(state.jetstream.clone(), freeze.clone());

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
                if let Err(e) = register(
                    &sched,
                    state.clone(),
                    &registered,
                    s.clone(),
                    freeze.clone(),
                )
                .await
                {
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

    // 2. Watch — react to KV puts/deletes for the lifetime of the
    //    process. #502: wrapped in a reopen loop (the freeze-watcher
    //    pattern below) — the old single watch fell through to
    //    `pending()` when the stream ended, after which every POST /
    //    DELETE /api/schedules was accepted into KV but never
    //    (un)registered in the running scheduler until a backend
    //    restart, with no operator-visible error. On each reopen the
    //    registrations are reconciled against the bucket so edits
    //    that landed while the watch was down are caught up.
    let mut first_attach = true;
    loop {
        let mut watcher = match kv.watch_all().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "schedules watch_all failed; retrying");
                tokio::time::sleep(StdDuration::from_secs(5)).await;
                continue;
            }
        };
        // Reconcile AFTER the watch is armed, so an edit landing in
        // the gap is seen by at least one of the two (re-applying a
        // registration is an idempotent replace). The first attach
        // skips it — step 1 above just did the initial load.
        if first_attach {
            first_attach = false;
        } else if let Err(e) =
            reconcile_registrations(&kv, &sched, &state, &registered, &freeze).await
        {
            warn!(error = %e, "schedules reconcile after watch reopen failed");
        }
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
                        && let Err(e) = register(
                            &sched,
                            state.clone(),
                            &registered,
                            sched_data.clone(),
                            freeze.clone(),
                        )
                        .await
                    {
                        warn!(error = %e, schedule_id = %sched_data.id, "watch register failed");
                    }
                }
                Operation::Delete | Operation::Purge => {
                    unregister(&sched, &registered, &entry.key).await;
                }
            }
        }
        warn!("schedules watch ended; reopening");
        tokio::time::sleep(StdDuration::from_secs(5)).await;
    }
}

/// #502: re-sync the in-memory registrations with the bucket after a
/// watch reopen. Every schedule currently in KV is re-applied
/// (unregister + register — the same idempotent replace the Put arm
/// does), and any registration whose KV entry disappeared while the
/// watch was down is dropped, so deletes can't survive a watch gap.
async fn reconcile_registrations(
    kv: &async_nats::jetstream::kv::Store,
    sched: &JobScheduler,
    state: &AppState,
    registered: &Registered,
    freeze: &FreezeMirror,
) -> Result<()> {
    // async-nats 0.48 gotcha (same as the initial load above):
    // `kv.keys()` errors on a bucket whose stream has zero messages.
    // Gate on the stream's message count so a genuinely empty bucket
    // reconciles to "drop everything" instead of erroring out — while
    // a broker-down `status()` failure still propagates as Err, which
    // keeps the existing registrations (review PR #549, gemini). A
    // bucket whose keys were all deleted carries tombstones
    // (messages > 0), so it takes the normal keys() path and yields
    // an empty live set, which is equally correct.
    let status = kv.status().await.context("schedules kv status")?;
    let keys: Vec<String> = if status.values() == 0 {
        Vec::new()
    } else {
        kv.keys()
            .await
            .context("list schedules keys")?
            .try_collect()
            .await
            .context("collect schedules keys")?
    };
    let mut seen: HashSet<String> = HashSet::new();
    for k in &keys {
        let bytes = match kv.get(k).await {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, key = %k, "reconcile kv get");
                continue;
            }
        };
        match serde_json::from_slice::<Schedule>(&bytes) {
            Ok(s) => {
                seen.insert(s.id.clone());
                unregister(sched, registered, &s.id).await;
                if s.enabled
                    && let Err(e) =
                        register(sched, state.clone(), registered, s.clone(), freeze.clone()).await
                {
                    warn!(error = %e, schedule_id = %s.id, "reconcile register failed");
                }
            }
            Err(e) => warn!(error = %e, key = %k, "deserialize Schedule on reconcile"),
        }
    }
    let stale: Vec<String> = registered
        .lock()
        .await
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    for id in stale {
        info!(schedule_id = %id, "reconcile: dropping registration deleted while watch was down");
        unregister(sched, registered, &id).await;
    }
    Ok(())
}

async fn register(
    sched: &JobScheduler,
    state: AppState,
    registered: &Registered,
    schedule: Schedule,
    freeze: FreezeMirror,
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
        let freeze = freeze.clone();
        Box::pin(async move {
            tick(&state, schedule, &freeze).await;
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
async fn tick(state: &AppState, schedule: Schedule, freeze: &FreezeMirror) {
    let schedule_id = schedule.id.clone();
    let job_id = schedule.job_id.clone();
    let lowered = schedule.lowered();

    // 0-) Fleet-wide change-freeze (#418 Phase 5). The most global
    //     gate, so it runs first — a frozen fleet shouldn't even
    //     resolve catalogs or targets. Read from the in-memory mirror
    //     (kept fresh by `spawn_freeze_watcher`) so the hot path never
    //     blocks on a KV get (gemini #472). Clone the reason out under
    //     the read lock, then release before logging/returning.
    let frozen_reason = {
        let guard = freeze.read().await;
        guard
            .as_ref()
            .filter(|f| f.is_active(Utc::now()))
            .map(|f| f.reason.clone())
    };
    if let Some(reason) = frozen_reason {
        tracing::info!(
            %schedule_id,
            reason = reason.as_deref().unwrap_or(""),
            "scheduler tick: fleet change-freeze active — skip",
        );
        return;
    }

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
    // #418 Phase 4: lower the schedule's on_failure.retry once — it's
    // Copy, so each dispatch below stamps the same spec onto its
    // Commands without re-borrowing `schedule` after `manifest` moves.
    let retry = schedule.on_failure.lowered_retry();

    // 2) For EveryTick (a calendar time trigger) we don't need to
    //    resolve anything — fire and forget. Skip the more expensive
    //    policy path entirely.
    if matches!(lowered.mode, ExecMode::EveryTick) {
        dispatch(
            state,
            &schedule_id,
            manifest,
            plan_for_dispatch(),
            retry,
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
    let cooldown = match parse_cooldown(lowered.cooldown.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            warn!(%schedule_id, error = %e, "scheduler fire failed: invalid when.every");
            return;
        }
    };
    // #487: bound the completion scan by the cooldown window — a
    // completion older than `now - cooldown` is re-armed by
    // definition and can never suppress, so reading it is wasted
    // I/O. This query runs every POLL_CRON minute per schedule, so
    // without the bound it walked the job's entire completion
    // history each tick. Cooldown-less schedules (`per_pc: once`
    // kitting semantics — "succeeded ever ⇒ permanently skip") keep
    // the unbounded scan: any historical completion matters there,
    // and the #486 retention now caps the table at 90 d anyway.
    let completion_cutoff = cooldown.map(|cd| now - cd);
    let completions = match recent_completions(state, &job_id, completion_cutoff).await {
        Ok(v) => v,
        Err(e) => {
            warn!(%schedule_id, error = ?e, "scheduler fire failed: completion lookup");
            return;
        }
    };

    let action = decide_fire(lowered.mode, cooldown, &expected, &completions, now);

    // Layer bounded in-flight suppression on top of the completion-
    // based decision: a PC (or the whole target) we already dispatched
    // within `window` — but whose completion hasn't reached
    // `execution_results` yet — stays muted, so the 1-minute POLL_CRON
    // doesn't re-fire it every tick across the jitter + run + drain
    // gap. Only the surviving subset reads its marks (cheap), and the
    // window self-expires so a dispatch that produced no completion
    // re-arms on its own.
    let window = suppress_window(&schedule, &manifest);
    let action = match action {
        FireAction::Skip => FireAction::Skip,
        FireAction::FireWholeTarget => {
            let target_mark = read_target_dispatch_mark(state, &schedule_id).await;
            suppress_dispatched(
                FireAction::FireWholeTarget,
                &HashMap::new(),
                target_mark,
                window,
                now,
            )
        }
        FireAction::FirePcs(pcs) => {
            let marks = read_pc_dispatch_marks(state, &schedule_id, &pcs).await;
            suppress_dispatched(FireAction::FirePcs(pcs), &marks, None, window, now)
        }
    };

    match action {
        FireAction::Skip => {
            tracing::debug!(
                %schedule_id, when = %schedule.when,
                expected = expected.len(),
                completions = completions.len(),
                "scheduler tick: dedup/in-flight says skip",
            );
        }
        FireAction::FireWholeTarget => {
            if dispatch(
                state,
                &schedule_id,
                manifest,
                plan_for_dispatch(),
                retry,
                "OncePerTarget armed",
            )
            .await
            {
                record_target_dispatch_mark(state, &schedule_id, now).await;
            }
        }
        FireAction::FirePcs(pc_ids) => {
            // #418 constraints.max_concurrent: cap how many instances of
            // this job run at once. Count the job's still-in-flight runs
            // and only fire at as many of the remaining pcs as there are
            // free slots; the rest wait for a later tick, which refills
            // slots as runs complete (rolling window). Known limit
            // (#418 (ii)): a just-dispatched run hasn't landed an
            // `events.started` row yet, so with `jitter` longer than the
            // 1-min poll the count can briefly lag and over-shoot the
            // cap; with no/short jitter (the common case) the run lands
            // before the next tick and the cap holds.
            let pc_ids = if let Some(max) = schedule.constraints.max_concurrent {
                let in_flight = count_in_flight(state, &job_id).await;
                let capped = cap_pcs_to_slots(pc_ids, in_flight, max);
                if capped.is_empty() {
                    tracing::debug!(
                        %schedule_id, %max, in_flight,
                        "max_concurrent: all slots busy — deferring this tick",
                    );
                    return;
                }
                capped
            } else {
                pc_ids
            };

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
            if dispatch(
                state,
                &schedule_id,
                manifest,
                plan,
                retry,
                "OncePerPc subset",
            )
            .await
            {
                record_pc_dispatch_marks(state, &schedule_id, &pc_ids, now).await;
            }
        }
    }
}

/// Returns `true` when the exec was accepted, so the caller can record
/// the dispatch mark only for fires that actually went out — a rejected
/// exec leaves the PC/target armed for the next tick.
async fn dispatch(
    state: &AppState,
    schedule_id: &str,
    manifest: Manifest,
    plan: FanoutPlan,
    retry: Option<kanade_shared::wire::RetrySpec>,
    why: &str,
) -> bool {
    match exec_manifest(state, manifest, plan, "scheduler", None, retry).await {
        Ok(resp) => {
            info!(
                %schedule_id, exec_id = %resp.exec_id, why,
                "scheduler exec ok",
            );
            true
        }
        Err((status, msg)) => {
            warn!(
                %schedule_id, status = %status, error = %msg, why,
                "scheduler exec failed",
            );
            false
        }
    }
}

/// Decode a fleet-freeze blob, failing *safe* on corruption: a
/// mangled value becomes a default (empty-window = always-active)
/// [`Freeze`] so the schedulers skip rather than punch through a
/// freeze the operator clearly set (mirrors the constraints.window
/// fail-closed direction).
fn parse_freeze_or_safe(bytes: &[u8]) -> Freeze {
    serde_json::from_slice::<Freeze>(bytes).unwrap_or_else(|e| {
        warn!(error = %e, "fleet freeze blob is corrupt — failing safe (treating fleet as frozen)");
        Freeze::default()
    })
}

/// One-shot read of the current fleet change-freeze (#418 Phase 5)
/// from the `fleet_config`/`freeze` KV singleton. `Ok(None)` ⇒ not
/// frozen (key absent); `Err` ⇒ the read itself failed (broker
/// trouble) — the caller must NOT conflate that with "not frozen"
/// (coderabbit #472). Used to seed the mirror at boot; thereafter
/// [`spawn_freeze_watcher`] keeps it fresh.
async fn load_freeze(js: &async_nats::jetstream::Context) -> Result<Option<Freeze>> {
    let kv = js
        .get_key_value(BUCKET_FLEET_CONFIG)
        .await
        .context("open fleet_config KV")?;
    match kv.get(KEY_FREEZE).await.context("get freeze key")? {
        Some(bytes) => Ok(Some(parse_freeze_or_safe(&bytes))),
        None => Ok(None),
    }
}

/// Keep the [`FreezeMirror`] current by tailing the `fleet_config` KV
/// (#418 Phase 5). A `put` on `KEY_FREEZE` refreshes it; a `delete`
/// clears it (= thawed). Reopens the watch on any stream error so a
/// transient broker hiccup doesn't leave the mirror permanently stale.
/// This replaces a per-tick `get_key_value` on the hot path (gemini
/// #472) — `tick` only reads the in-memory Arc.
fn spawn_freeze_watcher(js: async_nats::jetstream::Context, freeze: FreezeMirror) {
    tokio::spawn(async move {
        loop {
            let kv = match js.get_key_value(BUCKET_FLEET_CONFIG).await {
                Ok(kv) => kv,
                Err(e) => {
                    warn!(error = %e, "freeze watcher: fleet_config KV unavailable; retrying");
                    tokio::time::sleep(StdDuration::from_secs(5)).await;
                    continue;
                }
            };
            let mut watch = match kv.watch_all().await {
                Ok(w) => w,
                Err(e) => {
                    warn!(error = %e, "freeze watcher: watch_all failed; retrying");
                    tokio::time::sleep(StdDuration::from_secs(5)).await;
                    continue;
                }
            };
            while let Some(entry) = watch.next().await {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(error = %e, "freeze watcher: watch entry error; reopening");
                        break;
                    }
                };
                if entry.key != KEY_FREEZE {
                    continue;
                }
                let next = match entry.operation {
                    Operation::Put => Some(parse_freeze_or_safe(&entry.value)),
                    Operation::Delete | Operation::Purge => None,
                };
                let frozen = next.is_some();
                *freeze.write().await = next;
                info!(frozen, "fleet change-freeze mirror updated");
            }
            // watch ended (None) — reopen.
        }
    });
}

/// Count the still-in-flight runs of a job (#418 constraints
/// .max_concurrent) — `execution_results` rows for `job_id` whose
/// `finished_at` is still NULL (started, not yet returned; the reaper
/// stamps `finished_at` when it gives up on an orphan, so reaped rows
/// are excluded). Only called on a capped schedule's fire path (not
/// every tick), and `COUNT … WHERE finished_at IS NULL` is served by
/// the partial in-flight index, scanning only the small running set.
/// Best-effort: a DB error reads as 0 in-flight (warn), which fails
/// *open* — better to over-dispatch a capped job than to wedge it
/// shut on a transient query error.
async fn count_in_flight(state: &AppState, job_id: &str) -> u32 {
    match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM execution_results WHERE job_id = ? AND finished_at IS NULL",
    )
    .bind(job_id)
    .fetch_one(&state.pool)
    .await
    {
        // `try_from` instead of `as`: a fleet large enough to overflow
        // u32 in-flight runs is impossible, but the saturating convert
        // avoids the silent-truncation footgun (gemini #542).
        Ok(n) => u32::try_from(n).unwrap_or(u32::MAX),
        Err(e) => {
            warn!(error = %e, %job_id, "max_concurrent: in-flight count query failed; treating as 0");
            0
        }
    }
}

/// Truncate a per-pc fire list to the free slots left under a
/// `max_concurrent` cap: `slots = max - in_flight`, clamped at 0.
/// Pure so the rolling-window arithmetic is unit-tested without a DB.
/// The pcs kept are an arbitrary prefix — fairness across ticks comes
/// from the dispatched ones being suppressed next tick, so the
/// remainder gets its turn as slots free up.
fn cap_pcs_to_slots(mut pcs: Vec<String>, in_flight: u32, max_concurrent: u32) -> Vec<String> {
    let slots = max_concurrent.saturating_sub(in_flight) as usize;
    pcs.truncate(slots);
    pcs
}

/// In-flight suppression window for one schedule's dispatch marks —
/// see [`policy::suppress_dispatched`]. Sized to cover the worst-case
/// time from dispatch to a completion landing: agent-side `jitter` +
/// the script's own `timeout` + [`DISPATCH_DRAIN_MARGIN`]. Clamped to
/// `[DISPATCH_WINDOW_MIN, DISPATCH_WINDOW_MAX]` so a malformed or
/// outsized humantime can't push it to either extreme.
fn suppress_window(schedule: &Schedule, manifest: &Manifest) -> ChronoDuration {
    let parse = |s: &str| {
        humantime::parse_duration(s)
            .ok()
            .and_then(|d| ChronoDuration::from_std(d).ok())
    };
    let jitter = schedule
        .plan
        .jitter
        .as_deref()
        .and_then(parse)
        .unwrap_or_else(ChronoDuration::zero);
    let timeout = parse(&manifest.execute.timeout).unwrap_or_else(|| {
        // `execute.timeout` is validated at job-create time, so this is
        // effectively unreachable — but a malformed value silently
        // collapsing the window to `jitter + margin` could let a
        // long-running job re-fire mid-run, so make it detectable
        // instead of failing quietly (claude #444).
        warn!(
            job_id = %manifest.id,
            raw = %manifest.execute.timeout,
            "suppress_window: unparseable timeout; treating as zero",
        );
        ChronoDuration::zero()
    });
    // #418 Phase 4: on_failure.retry lets a single fire re-run the
    // script up to `max` extra times with `backoff` between, so the
    // worst-case legitimate run grows by `max * (timeout + backoff)`.
    // Without this the suppression window would expire mid-retry and
    // the next poll tick would re-dispatch the still-running schedule
    // (gemini CRITICAL / coderabbit MAJOR #466). `max` is bounded
    // (1..=10 via validate), so the budget is finite and operator-
    // chosen.
    let retry_budget = schedule
        .on_failure
        .lowered_retry()
        .and_then(|r| {
            let per_attempt =
                timeout.checked_add(&ChronoDuration::seconds(r.backoff_secs as i64))?;
            per_attempt.checked_mul(r.max as i32)
        })
        .unwrap_or_else(ChronoDuration::zero);
    // checked_add: `from_std` already rejects out-of-range humantime, so
    // overflow is unreachable in practice — but fall back to the ceiling
    // rather than panic if some future input ever does overflow (gemini #444).
    // The jitter+timeout+margin part keeps its historical [MIN, MAX]
    // clamp (a runaway jitter/timeout must not mute a schedule forever
    // on a dispatch that may have gone nowhere). The retry budget is
    // then added on top: it's an operator-bounded, legitimate run
    // duration, so it extends the window past the default ceiling
    // rather than being clamped away mid-retry (#466).
    let plain = jitter
        .checked_add(&timeout)
        .and_then(|d| d.checked_add(&DISPATCH_DRAIN_MARGIN))
        .map(|d| d.clamp(DISPATCH_WINDOW_MIN, DISPATCH_WINDOW_MAX))
        .unwrap_or(DISPATCH_WINDOW_MAX);
    plain.checked_add(&retry_budget).unwrap_or(plain)
}

/// Decode a dispatch mark (RFC3339 bytes). A missing / unparsable value
/// is treated as "no mark" by the callers, which fails open to firing —
/// the completion-based dedup stays the correctness backstop.
fn parse_dispatch_mark(bytes: &[u8]) -> Option<DateTime<Utc>> {
    let s = std::str::from_utf8(bytes).ok()?;
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Best-effort read of the per-pc dispatch marks for `pcs`. A missing
/// bucket / key just yields no entry for that pc (→ not suppressed).
async fn read_pc_dispatch_marks(
    state: &AppState,
    schedule_id: &str,
    pcs: &[String],
) -> HashMap<String, DateTime<Utc>> {
    let Ok(kv) = state
        .jetstream
        .get_key_value(BUCKET_SCHEDULER_DISPATCH)
        .await
    else {
        return HashMap::new();
    };
    // Run the per-pc gets concurrently — a `target: all` fleet can be
    // thousands of pcs and serial round-trips would stall the tick
    // (gemini #444).
    futures::stream::iter(pcs.iter().cloned())
        .map(|pc| {
            let kv = kv.clone();
            let key = dispatch_mark_pc_key(schedule_id, &pc);
            async move {
                let ts = match kv.get(&key).await {
                    Ok(Some(bytes)) => parse_dispatch_mark(&bytes),
                    _ => None,
                };
                (pc, ts)
            }
        })
        .buffer_unordered(DISPATCH_KV_CONCURRENCY)
        .filter_map(|(pc, ts)| async move { ts.map(|t| (pc, t)) })
        .collect()
        .await
}

/// Best-effort read of the whole-target dispatch mark.
async fn read_target_dispatch_mark(state: &AppState, schedule_id: &str) -> Option<DateTime<Utc>> {
    let kv = state
        .jetstream
        .get_key_value(BUCKET_SCHEDULER_DISPATCH)
        .await
        .ok()?;
    let bytes = kv
        .get(&dispatch_mark_target_key(schedule_id))
        .await
        .ok()??;
    parse_dispatch_mark(&bytes)
}

/// Record per-pc dispatch marks after a OncePerPc fire actually went
/// out. Best-effort: a failed write just means the next tick may
/// re-fire (the prior, over-firing behavior) for that pc — never wrong,
/// only chattier.
async fn record_pc_dispatch_marks(
    state: &AppState,
    schedule_id: &str,
    pcs: &[String],
    at: DateTime<Utc>,
) {
    let Ok(kv) = state
        .jetstream
        .get_key_value(BUCKET_SCHEDULER_DISPATCH)
        .await
    else {
        warn!(%schedule_id, "record dispatch marks: scheduler_dispatch KV unavailable");
        return;
    };
    let val = at.to_rfc3339();
    // Concurrent writes, same rationale as read_pc_dispatch_marks
    // (gemini #444).
    futures::stream::iter(pcs.iter().cloned())
        .for_each_concurrent(DISPATCH_KV_CONCURRENCY, |pc| {
            let kv = kv.clone();
            let key = dispatch_mark_pc_key(schedule_id, &pc);
            let val = val.clone();
            async move {
                if let Err(e) = kv.put(&key, val.into_bytes().into()).await {
                    warn!(%schedule_id, pc, error = %e, "record dispatch mark failed");
                }
            }
        })
        .await;
}

/// Record the whole-target dispatch mark after a OncePerTarget fire.
async fn record_target_dispatch_mark(state: &AppState, schedule_id: &str, at: DateTime<Utc>) {
    let Ok(kv) = state
        .jetstream
        .get_key_value(BUCKET_SCHEDULER_DISPATCH)
        .await
    else {
        warn!(%schedule_id, "record target dispatch mark: scheduler_dispatch KV unavailable");
        return;
    };
    let key = dispatch_mark_target_key(schedule_id);
    if let Err(e) = kv.put(&key, at.to_rfc3339().into_bytes().into()).await {
        warn!(%schedule_id, error = %e, "record target dispatch mark failed");
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
///
/// #487: `cutoff` bounds the scan to completions that can still
/// suppress a fire. `Some(now - cooldown)` for cooldown-ful
/// schedules (older completions are re-armed by definition);
/// `None` for cooldown-less `once` semantics, where any historical
/// completion permanently suppresses. Note the #486/#541 retention
/// interaction: completions older than the 90 d sweep are deleted,
/// so a cooldown-less `once` schedule re-fires a PC whose only
/// completion aged out — acceptable for the idempotent
/// kitting-once jobs that mode exists for.
async fn recent_completions(
    state: &AppState,
    job_id: &str,
    cutoff: Option<DateTime<Utc>>,
) -> Result<Vec<Completion>> {
    // Branched queries instead of `(? IS NULL OR ...)` — the OR form
    // can stop SQLite driving an index from the bound column, and
    // this runs every poll minute per schedule (PR #557 review,
    // gemini).
    let query = if let Some(cutoff) = cutoff {
        sqlx::query(
            "SELECT pc_id, MAX(finished_at) AS finished_at
             FROM execution_results
             WHERE job_id = ? AND exit_code = 0 AND finished_at >= ?
             GROUP BY pc_id",
        )
        .bind(job_id)
        .bind(cutoff)
    } else {
        sqlx::query(
            "SELECT pc_id, MAX(finished_at) AS finished_at
             FROM execution_results
             WHERE job_id = ? AND exit_code = 0
             GROUP BY pc_id",
        )
        .bind(job_id)
    };
    let rows = query
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
///
/// `pub(crate)` since #485: `exec_manifest` uses the same resolution
/// to stamp `executions.target_count` with the number of PCs
/// expected to reply, instead of the number of NATS subjects
/// published (which flipped broadcast execs to 'completed' after the
/// first reply).
pub(crate) async fn resolve_expected_pcs(state: &AppState, target: &Target) -> Result<Vec<String>> {
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
            // #487: bounded-concurrency membership reads over the
            // ALIVE set directly — kv.keys() was an expensive
            // stream-wide ordered-consumer scan, and alive PCs are
            // the only ones we ever admit anyway (PR #557 review,
            // gemini). Concurrency matches the dispatch-mark reads
            // (gemini #444).
            let members: Vec<String> = futures::stream::iter(alive)
                .map(|k| {
                    let kv = kv.clone();
                    async move {
                        let Ok(Some(bytes)) = kv.get(&k).await else {
                            return None;
                        };
                        // The bucket stores the AgentGroups wrapper,
                        // not a bare Vec<String> — the old bare parse
                        // failed on every entry, so OncePerPc group
                        // targets silently resolved no members via
                        // this path (PR #557 review, CodeRabbit).
                        let Ok(value) = serde_json::from_slice::<AgentGroups>(&bytes) else {
                            return None;
                        };
                        Some((k, value.groups))
                    }
                })
                .buffer_unordered(DISPATCH_KV_CONCURRENCY)
                .filter_map(|res| async move { res })
                .filter_map(|(k, groups)| {
                    let is_member = groups.iter().any(|g| want.contains(g.as_str()));
                    async move { is_member.then_some(k) }
                })
                .collect()
                .await;
            out.extend(members);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with_timeout(timeout: &str) -> Manifest {
        serde_yaml::from_str(&format!(
            "id: j\nversion: 1.0.0\nexecute:\n  shell: powershell\n  script: \"echo hi\"\n  timeout: {timeout}\n"
        ))
        .expect("manifest parse")
    }

    fn schedule_with(extra: &str) -> Schedule {
        serde_yaml::from_str(&format!(
            "id: s\nwhen:\n  per_pc: {{ every: 6h }}\njob_id: j\ntarget: {{ all: true }}\n{extra}"
        ))
        .expect("schedule parse")
    }

    #[test]
    fn suppress_window_includes_retry_budget() {
        // #418 Phase 4: the in-flight suppression window must budget for
        // retry time so a retrying fire isn't re-dispatched mid-run
        // (gemini CRITICAL / coderabbit MAJOR #466).
        let manifest = manifest_with_timeout("30s");
        let plain = suppress_window(&schedule_with(""), &manifest);
        let with_retry = suppress_window(
            &schedule_with("on_failure:\n  retry: { max: 3, backoff: 1m }\n"),
            &manifest,
        );
        // budget = 3 * (30s + 60s) = 270s on top of the plain window.
        assert!(
            with_retry >= plain + ChronoDuration::seconds(270),
            "retry window {with_retry:?} must exceed plain {plain:?} by the budget",
        );
    }

    #[test]
    fn suppress_window_lifts_ceiling_for_long_retry() {
        // max:10 backoff:10m timeout:5m → 10*(15m) = 150m, far past the
        // 30-min default ceiling; the cap must lift, else a legitimately
        // long retry sequence gets re-fired mid-run.
        let manifest = manifest_with_timeout("5m");
        let w = suppress_window(
            &schedule_with("on_failure:\n  retry: { max: 10, backoff: 10m }\n"),
            &manifest,
        );
        assert!(
            w > DISPATCH_WINDOW_MAX,
            "window {w:?} must exceed the default cap"
        );
    }

    #[test]
    fn suppress_window_without_retry_keeps_default_clamp() {
        // No retry → the historical jitter+timeout+margin window,
        // clamped to the 30-min ceiling (regression guard).
        let manifest = manifest_with_timeout("1h");
        let w = suppress_window(&schedule_with(""), &manifest);
        assert_eq!(w, DISPATCH_WINDOW_MAX);
    }

    // ---- constraints.max_concurrent (#418) ----

    fn pcs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("pc-{i}")).collect()
    }

    #[test]
    fn cap_truncates_to_free_slots() {
        // max 5, 2 already running → 3 free slots.
        let out = cap_pcs_to_slots(pcs(10), 2, 5);
        assert_eq!(out.len(), 3);
        assert_eq!(out, vec!["pc-0", "pc-1", "pc-2"]);
    }

    #[test]
    fn cap_at_capacity_defers_all() {
        // in-flight == cap → 0 slots → empty (caller skips the tick).
        assert!(cap_pcs_to_slots(pcs(4), 5, 5).is_empty());
        // over capacity (a transient over-shoot) → saturating to 0.
        assert!(cap_pcs_to_slots(pcs(4), 9, 5).is_empty());
    }

    #[test]
    fn cap_under_slots_keeps_all() {
        // Fewer eligible pcs than free slots → fire all of them.
        let out = cap_pcs_to_slots(pcs(2), 0, 5);
        assert_eq!(out.len(), 2);
    }
}
