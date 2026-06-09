//! v0.23: agent-side cron scheduler for `runs_on: agent` schedules.
//!
//! When the operator marks a schedule `runs_on: agent`, the backend's
//! central scheduler steps out and leaves the definition in
//! `BUCKET_SCHEDULES` for each targeted agent to pick up. This
//! module is the agent-side counterpart: it watches the same KV,
//! filters for schedules whose target matches this agent and whose
//! `runs_on` is `Agent`, and runs an internal `tokio_cron_scheduler`
//! for them.
//!
//! On a local tick the agent looks up the Manifest from a small
//! locally-cached snapshot of `BUCKET_JOBS`, applies the mode-based
//! dedup against `<data_dir>/local_completions.json`, builds a
//! Command, and runs it through the same `handle_command` path that
//! the live-NATS Commands use — so kill / cooldown / inventory
//! projection all behave identically.
//!
//! What we don't yet do (v0.24 territory):
//!
//! * Full outbox for results when the broker is unreachable — we
//!   rely on async-nats client buffering, which handles seconds-to-
//!   minutes outages but won't survive a multi-day air-gap.
//! * Group membership reflection — we re-read `agent_groups` once
//!   per schedule-KV change. Group churn in between is missed until
//!   the next schedule edit.
//!
//! Both are gated on this feature actually being exercised in the
//! field; ship the minimum that's useful today.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_nats::jetstream::kv::Operation;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::{StreamExt, TryStreamExt};
use kanade_shared::kv::{
    BUCKET_JOBS, BUCKET_SCHEDULES, BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS,
};
use kanade_shared::manifest::{ExecMode, Manifest, RunsOn, Schedule, ScheduleTz, When};
use kanade_shared::wire::Command;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::commands::handle_command;
use crate::nats_retry;
use crate::script_cache::ScriptCache;

/// A Manifest plus any pre-resolved metadata `local_tick` needs to
/// build a Command without touching the broker.
///
/// `script_object_sha256` is populated at `apply_resync` time (when
/// the broker is by definition reachable — we just listed
/// BUCKET_JOBS); `local_tick` reads it from cache so a
/// `runs_on: agent` schedule keeps firing `script_object:`
/// manifests after the broker goes away.
///
/// `None` covers two cases:
///   - inline-`script:` manifests (no digest needed)
///   - `script_object:` manifests whose digest fetch failed at
///     the last resync (broker race, bucket missing, …); these
///     skip the tick the same way pre-cache code did
#[derive(Clone, Debug)]
struct ResolvedJob {
    manifest: Manifest,
    /// Lowercase hex sha256 the agent's script_cache will verify
    /// fetched bytes against. Only set when `manifest.execute
    /// .script_object` is `Some` AND `digest_of` succeeded.
    script_object_sha256: Option<String>,
}

/// In-memory state shared across the watch loops and the tick
/// callbacks. Wrapped in a single `Mutex<State>` because the scheduler
/// only ticks one job at a time and the watch loops are also serial.
struct State {
    /// Latest snapshot of every job in BUCKET_JOBS plus any
    /// pre-resolved script_object digest (Gemini #214 HIGH fix —
    /// keeps `local_tick` offline-tolerant by removing its network
    /// round-trip).
    jobs: HashMap<String, ResolvedJob>,
    /// schedule_id → internal cron Uuid (for removing the Job).
    registered: HashMap<String, Uuid>,
    /// schedule_id → cached Schedule (so the tick callback knows
    /// what it's running without re-fetching).
    schedules: HashMap<String, Schedule>,
    /// Last-success timestamps keyed by `<schedule_id>::<job_id>`,
    /// persisted to `local_completions.json`.
    completions: HashMap<String, DateTime<Utc>>,
    /// Path to the completions file (under agent's data dir).
    completions_path: PathBuf,
}

impl State {
    fn matching(&self, schedule: &Schedule, pc_id: &str, my_groups: &[String]) -> bool {
        matches!(schedule.runs_on, RunsOn::Agent)
            && schedule.enabled
            && target_includes(schedule, pc_id, my_groups)
    }

    fn key(schedule_id: &str, job_id: &str) -> String {
        format!("{schedule_id}::{job_id}")
    }

    fn record_completion(&mut self, schedule_id: &str, job_id: &str, when: DateTime<Utc>) {
        self.completions
            .insert(Self::key(schedule_id, job_id), when);
        if let Err(e) = self.flush_completions() {
            warn!(
                error = %e,
                "local_completions.json flush failed; in-memory state still consistent",
            );
        }
    }

    fn flush_completions(&self) -> Result<()> {
        let tmp = self.completions_path.with_extension("json.tmp");
        let bytes =
            serde_json::to_vec_pretty(&self.completions).context("serialise local_completions")?;
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&tmp, &bytes).context("write tmp completions file")?;
        std::fs::rename(&tmp, &self.completions_path).context("rename tmp → final")?;
        Ok(())
    }

    fn load_completions(path: &std::path::Path) -> HashMap<String, DateTime<Utc>> {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, path = %path.display(), "parse local_completions; starting empty");
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!(error = %e, path = %path.display(), "read local_completions; starting empty");
                HashMap::new()
            }
        }
    }
}

/// Does this schedule target the given agent? Pure function for
/// testability — `pc_id` and `my_groups` are the agent's own.
fn target_includes(schedule: &Schedule, pc_id: &str, my_groups: &[String]) -> bool {
    let t = &schedule.plan.target;
    if t.all {
        return true;
    }
    if t.pcs.iter().any(|p| p == pc_id) {
        return true;
    }
    if t.groups.iter().any(|g| my_groups.iter().any(|m| m == g)) {
        return true;
    }
    false
}

pub fn spawn(
    client: async_nats::Client,
    pc_id: String,
    completions_path: PathBuf,
    groups_rx: tokio::sync::watch::Receiver<Vec<String>>,
    staleness: crate::staleness::Tracker,
    script_cache: ScriptCache,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(
            client,
            pc_id,
            completions_path,
            groups_rx,
            staleness,
            script_cache,
        )
        .await;
    })
}

async fn run(
    client: async_nats::Client,
    pc_id: String,
    completions_path: PathBuf,
    groups_rx: tokio::sync::watch::Receiver<Vec<String>>,
    staleness: crate::staleness::Tracker,
    script_cache: ScriptCache,
) {
    let js = async_nats::jetstream::new(client.clone());

    // The internal scheduler doesn't talk to NATS, so it's created
    // unconditionally — even a broker-down boot lets `local_tick`
    // fire as soon as we've re-primed the cache after recovery.
    let internal = match JobScheduler::new().await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "local_scheduler: JobScheduler::new failed; aborting subsystem");
            return;
        }
    };
    if let Err(e) = internal.start().await {
        warn!(error = %e, "local_scheduler: JobScheduler::start failed; aborting subsystem");
        return;
    }

    let completions = State::load_completions(&completions_path);
    info!(
        path = %completions_path.display(),
        loaded = completions.len(),
        "local_scheduler: loaded completion state",
    );
    let state = Arc::new(Mutex::new(State {
        jobs: HashMap::new(),
        registered: HashMap::new(),
        schedules: HashMap::new(),
        completions,
        completions_path,
    }));

    // Long-lived auxiliary task: react to group-membership flips even
    // while the schedules / jobs watches are mid-reopen. Uses
    // `wait_for_kv` so a flip during a broker outage queues up
    // properly instead of being lost.
    let _groups_task = spawn_groups_change_task(
        client.clone(),
        pc_id.clone(),
        staleness.clone(),
        groups_rx.clone(),
        internal.clone(),
        state.clone(),
        script_cache.clone(),
    );

    // Outer reconnect loop. Owns schedules_kv + jobs_kv handles and
    // both `watch_all` streams; re-syncs caches + reconciles on
    // every (re-)entry so edits made during a disconnect get picked
    // up.
    loop {
        let schedules_kv = nats_retry::wait_for_kv(
            &js,
            &client,
            &staleness,
            BUCKET_SCHEDULES,
            "local_scheduler",
        )
        .await;
        let jobs_kv =
            nats_retry::wait_for_kv(&js, &client, &staleness, BUCKET_JOBS, "local_scheduler").await;

        // Walk both KVs into FRESH collections first. Don't touch
        // live state until both walks succeed end-to-end — a partial
        // failure must NOT clear the in-memory caches (Gemini #147
        // review: a transient keys() error would otherwise leave
        // the scheduler empty until the next watch event arrives).
        let new_jobs = match collect_jobs(&jobs_kv).await {
            Ok(j) => j,
            Err(()) => {
                warn!("local_scheduler: jobs KV walk failed; keeping previous state and reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        let new_schedules = match collect_schedules(&schedules_kv).await {
            Ok(s) => s,
            Err(()) => {
                warn!(
                    "local_scheduler: schedules KV walk failed; keeping previous state and reopening"
                );
                nats_retry::reopen_pause().await;
                continue;
            }
        };

        let my_groups = groups_rx.borrow().clone();
        info!(
            pc_id = %pc_id,
            groups = ?my_groups,
            jobs = new_jobs.len(),
            schedules = new_schedules.len(),
            "local_scheduler: applying resync",
        );
        apply_resync(
            &internal,
            &state,
            &client,
            &pc_id,
            &my_groups,
            &staleness,
            &script_cache,
            new_jobs,
            new_schedules,
        )
        .await;
        let count = state.lock().await.registered.len();
        info!(count, "local_scheduler: registered schedules after resync");

        let mut schedules_watch = match schedules_kv.watch_all().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "schedules KV watch_all failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        let mut jobs_watch = match jobs_kv.watch_all().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "jobs KV watch_all failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };

        // Inner select loop. `break` (with label) on either watch
        // dropping so we re-prime both together.
        let dropped = 'inner: loop {
            tokio::select! {
                entry = schedules_watch.next() => {
                    let Some(entry) = entry else { break 'inner "schedules" };
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => { warn!(error = %e, "schedules watch error"); continue; }
                    };
                    let groups_snapshot = groups_rx.borrow().clone();
                    match entry.operation {
                        Operation::Put => {
                            if let Ok(s) = serde_json::from_slice::<Schedule>(&entry.value) {
                                reconcile_schedule(
                                    &internal, &state, &client, &pc_id, &groups_snapshot, &s, &staleness, &script_cache,
                                )
                                .await;
                            } else {
                                warn!(key = %entry.key, "deserialize Schedule on watch");
                            }
                        }
                        Operation::Delete | Operation::Purge => {
                            unregister_locally(&internal, &state, &entry.key).await;
                        }
                    }
                }
                entry = jobs_watch.next() => {
                    let Some(entry) = entry else { break 'inner "jobs" };
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => { warn!(error = %e, "jobs watch error"); continue; }
                    };
                    match entry.operation {
                        Operation::Put => {
                            let Ok(m) = serde_json::from_slice::<Manifest>(&entry.value) else {
                                warn!(key = %entry.key, "local_scheduler: parse Manifest from jobs watch");
                                continue;
                            };
                            // Resolve digest BEFORE taking the lock —
                            // the call is a NATS round-trip and we
                            // don't want `local_tick` blocked behind
                            // it. Falls back to None on broker
                            // failure (tick skips that job until the
                            // next watch event succeeds).
                            let sha = match m.execute.script_object.as_deref() {
                                Some(key) => match script_cache.digest_of(key).await {
                                    Ok(d) => Some(d),
                                    Err(e) => {
                                        warn!(
                                            job_id = %entry.key,
                                            %key,
                                            error = %e,
                                            "jobs watch: digest fetch failed; caching manifest with digest=None",
                                        );
                                        None
                                    }
                                },
                                None => None,
                            };
                            let mut s = state.lock().await;
                            s.jobs.insert(
                                entry.key.clone(),
                                ResolvedJob { manifest: m, script_object_sha256: sha },
                            );
                            debug!(job_id = %entry.key, "local_scheduler: cached job manifest");
                        }
                        Operation::Delete | Operation::Purge => {
                            let mut s = state.lock().await;
                            s.jobs.remove(&entry.key);
                        }
                    }
                }
            }
        };
        warn!(dropped, "local_scheduler watch ended; reopening");
        nats_retry::reopen_pause().await;
    }
}

/// Walk `BUCKET_JOBS` into a fresh in-memory map. Returns `Err(())`
/// if `kv.keys()` itself fails — caller must treat that as
/// "connectivity-level failure, keep existing cache" rather than
/// "no jobs" (Gemini #147 review).
async fn collect_jobs(
    jobs_kv: &async_nats::jetstream::kv::Store,
) -> Result<HashMap<String, Manifest>, ()> {
    let keys = match jobs_kv.keys().await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "local_scheduler: jobs_kv.keys() failed");
            return Err(());
        }
    };
    let keys: Vec<String> = keys.try_collect().await.unwrap_or_default();
    let mut out = HashMap::with_capacity(keys.len());
    for k in keys {
        if let Ok(Some(bytes)) = jobs_kv.get(&k).await
            && let Ok(m) = serde_json::from_slice::<Manifest>(&bytes)
        {
            out.insert(k, m);
        }
    }
    Ok(out)
}

/// Walk `BUCKET_SCHEDULES` into a fresh list. Returns `Err(())` on
/// keys() failure — same rationale as [`collect_jobs`].
async fn collect_schedules(
    schedules_kv: &async_nats::jetstream::kv::Store,
) -> Result<Vec<Schedule>, ()> {
    let keys = match schedules_kv.keys().await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "local_scheduler: schedules_kv.keys() failed");
            return Err(());
        }
    };
    let keys: Vec<String> = keys.try_collect().await.unwrap_or_default();
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        if let Ok(Some(bytes)) = schedules_kv.get(&k).await
            && let Ok(s) = serde_json::from_slice::<Schedule>(&bytes)
        {
            out.push(s);
        }
    }
    Ok(out)
}

/// Atomically apply a fresh `new_jobs` / `new_schedules` snapshot.
/// Schedules that disappeared from KV (vs the in-memory cache) are
/// unregistered; remaining schedules are reconciled against the
/// new job manifests. Replaces the old `reset_state + prime` path
/// which would clear in-memory caches *before* trying to refill
/// them — a partial walk failure left the scheduler empty.
#[allow(clippy::too_many_arguments)]
async fn apply_resync(
    internal: &JobScheduler,
    state: &Arc<Mutex<State>>,
    client: &async_nats::Client,
    pc_id: &str,
    my_groups: &[String],
    staleness: &crate::staleness::Tracker,
    script_cache: &ScriptCache,
    new_jobs: HashMap<String, Manifest>,
    new_schedules: Vec<Schedule>,
) {
    // Resolve each manifest into a `ResolvedJob` — pre-fetch the
    // OBJECT_SCRIPTS digest for `script_object:` manifests so
    // `local_tick` reads it from cache (offline-tolerant; Gemini
    // #214 HIGH). Digest fetches happen here because we're already
    // talking to the broker — wait_for_kv returned the manifests
    // moments ago, so the digest_of call is on a warm path.
    //
    // A failed digest_of degrades to `script_object_sha256: None`,
    // which `local_tick` treats the same as "no cached digest" =
    // skip-with-warn. The manifest still gets cached so a later
    // resync with a healthier broker can populate the digest.
    //
    // Digests are resolved in parallel via `join_all` (Gemini #216
    // MED) so a fleet with many `script_object:` manifests doesn't
    // serialize N round-trips. Inline-only manifests skip the
    // network entirely — the async branch returns immediately.
    let resolve_futs = new_jobs.into_iter().map(|(id, manifest)| {
        let script_cache = script_cache.clone();
        async move {
            let script_object_sha256 = match manifest.execute.script_object.as_deref() {
                Some(key) => match script_cache.digest_of(key).await {
                    Ok(d) => Some(d),
                    Err(e) => {
                        warn!(
                            job_id = %id,
                            %key,
                            error = %e,
                            "apply_resync: script_object digest fetch failed; \
                             tick will skip until next successful resync",
                        );
                        None
                    }
                },
                None => None,
            };
            (
                id,
                ResolvedJob {
                    manifest,
                    script_object_sha256,
                },
            )
        }
    });
    let resolved: HashMap<String, ResolvedJob> = futures::future::join_all(resolve_futs)
        .await
        .into_iter()
        .collect();

    // Swap the jobs map atomically — under the lock so `local_tick`
    // sees either the old map in full or the new map in full, never
    // a half-cleared one.
    {
        let mut st = state.lock().await;
        st.jobs = resolved;
    }

    // Find schedules that vanished from KV → unregister them. Done
    // before the reconciliations so the diff is unambiguous.
    let new_ids: std::collections::HashSet<String> =
        new_schedules.iter().map(|s| s.id.clone()).collect();
    let stale_ids: Vec<String> = {
        let st = state.lock().await;
        st.schedules
            .keys()
            .filter(|id| !new_ids.contains(*id))
            .cloned()
            .collect()
    };
    for id in stale_ids {
        unregister_locally(internal, state, &id).await;
    }

    // Reconcile each schedule from the new snapshot. Updates the
    // cron registration in place where the schedule changed
    // (target / cron / enabled); no-ops where it's identical.
    for s in &new_schedules {
        reconcile_schedule(
            internal,
            state,
            client,
            pc_id,
            my_groups,
            s,
            staleness,
            script_cache,
        )
        .await;
    }
}

/// v0.24: group-membership change handler. Re-reconciles every
/// schedule the agent already knows about so `target.groups` overlap
/// re-evaluates without waiting for the next schedule edit. Uses
/// `wait_for_kv` so a flip during a broker outage queues up and
/// reconciles once the link is back instead of being silently
/// dropped (`groups_rx.changed()` is edge-triggered; if we miss the
/// edge by being mid-disconnect we never get it again).
///
/// When the schedules-KV walk fails (`collect_schedules` returns
/// `Err(())`), we skip the iteration and wait for the next group
/// flip — better to defer reconciliation than to interpret a
/// transient read failure as "schedules vanished" and drop every
/// agent-side cron (sub-agent #147 review).
#[allow(clippy::too_many_arguments)]
fn spawn_groups_change_task(
    client: async_nats::Client,
    pc_id: String,
    staleness: crate::staleness::Tracker,
    mut groups_rx_for_watch: tokio::sync::watch::Receiver<Vec<String>>,
    internal: JobScheduler,
    state: Arc<Mutex<State>>,
    script_cache: ScriptCache,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let js = async_nats::jetstream::new(client.clone());
        // Skip the initial value — already used in run()'s prime
        // pass. Future changes flow through here.
        loop {
            if groups_rx_for_watch.changed().await.is_err() {
                break;
            }
            let new_groups = groups_rx_for_watch.borrow().clone();
            info!(
                groups = ?new_groups,
                "local_scheduler: group membership changed; re-reconciling all schedules",
            );
            // Walk schedules KV again with retry semantics — a flip
            // during broker-down would otherwise be lost.
            let kv = nats_retry::wait_for_kv(
                &js,
                &client,
                &staleness,
                BUCKET_SCHEDULES,
                "local_scheduler_groups",
            )
            .await;
            let new_schedules = match collect_schedules(&kv).await {
                Ok(s) => s,
                Err(()) => {
                    warn!(
                        "local_scheduler: groups change resync — schedules walk failed; skipping iteration"
                    );
                    continue;
                }
            };
            // Compute the set of current schedules so we can drop
            // any that vanished. Done before reconciles so the diff
            // is unambiguous.
            let new_ids: std::collections::HashSet<String> =
                new_schedules.iter().map(|s| s.id.clone()).collect();
            let stale_ids: Vec<String> = {
                let st = state.lock().await;
                st.schedules
                    .keys()
                    .filter(|id| !new_ids.contains(*id))
                    .cloned()
                    .collect()
            };
            for id in stale_ids {
                unregister_locally(&internal, &state, &id).await;
            }
            for s in &new_schedules {
                reconcile_schedule(
                    &internal,
                    &state,
                    &client,
                    &pc_id,
                    &new_groups,
                    s,
                    &staleness,
                    &script_cache,
                )
                .await;
            }
        }
    })
}

// v0.24: `read_my_groups` removed — membership now flows through the
// `groups::spawn` watch channel that `local_scheduler` subscribes to,
// so we no longer poll the KV ourselves.

/// Reconcile a single schedule: drop any existing cron registration
/// for the same id, then re-register it if it targets this agent.
///
/// Holds `state.lock()` for the entire body — including across the
/// async `internal.remove()` and `internal.add()` calls. This is
/// deliberate: two concurrent callers (the inner watch loop and
/// `spawn_groups_change_task`) can otherwise interleave their
/// `internal.add` calls and leave two cron entries for the same
/// schedule_id in the scheduler while `state.registered` records
/// only the second uuid — an orphaned cron that double-fires every
/// tick until the agent restarts (sub-agent #147 review F1).
///
/// The lock-across-await is supported by `tokio::sync::Mutex` and
/// is acceptable here because reconciles are infrequent (per Put
/// event from the schedules KV watch, or per group-change flip).
/// The cron callback (`local_tick`) also locks `state`, but it does
/// so briefly and only inside the tick handler — never while
/// reconcile is running, since reconcile holds the lock for ~ms
/// (internal.add is in-memory).
#[allow(clippy::too_many_arguments)]
async fn reconcile_schedule(
    internal: &JobScheduler,
    state: &Arc<Mutex<State>>,
    client: &async_nats::Client,
    pc_id: &str,
    my_groups: &[String],
    schedule: &Schedule,
    staleness: &crate::staleness::Tracker,
    script_cache: &ScriptCache,
) {
    let mut st = state.lock().await;
    let mine = st.matching(schedule, pc_id, my_groups);

    // Always unregister an existing copy first — cron / target /
    // enabled edits all need to land.
    if let Some(uuid) = st.registered.remove(&schedule.id) {
        st.schedules.remove(&schedule.id);
        if let Err(e) = internal.remove(&uuid).await {
            warn!(error = %e, schedule_id = %schedule.id, "local_scheduler: remove failed");
        } else {
            info!(schedule_id = %schedule.id, "local_scheduler: unregistered");
        }
    }

    if !mine {
        return;
    }

    // #418: lower `when` onto the engine cron — POLL_CRON for
    // reconcile shapes, a 6/7-field cron for calendar shapes.
    // Phase 2: evaluated in the schedule's tz via new_async_tz
    // (Local = this agent's TZ, the natural "tz: local" meaning).
    let lowered = schedule.lowered();
    let cron = lowered.cron;
    let schedule_id = schedule.id.clone();
    let client_for_job = client.clone();
    let pc_id_for_job = pc_id.to_string();
    let state_for_job = state.clone();
    let schedule_for_job = schedule.clone();
    let staleness_for_job = staleness.clone();
    let script_cache_for_job = script_cache.clone();
    let cb = move |_uuid, _l| {
        let client = client_for_job.clone();
        let pc_id = pc_id_for_job.clone();
        let state = state_for_job.clone();
        let schedule = schedule_for_job.clone();
        let staleness = staleness_for_job.clone();
        let script_cache = script_cache_for_job.clone();
        Box::pin(async move {
            local_tick(
                &client,
                &pc_id,
                &state,
                &schedule,
                &staleness,
                &script_cache,
            )
            .await;
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    };
    let built = match lowered.tz {
        ScheduleTz::Utc => Job::new_async_tz(cron.as_str(), chrono::Utc, cb),
        ScheduleTz::Local => Job::new_async_tz(cron.as_str(), chrono::Local, cb),
    };
    let job = match built {
        Ok(j) => j,
        Err(e) => {
            warn!(
                schedule_id = %schedule.id,
                error = %e,
                "local_scheduler: Job::new_async_tz failed",
            );
            return;
        }
    };
    let job_uuid = match internal.add(job).await {
        Ok(u) => u,
        Err(e) => {
            warn!(
                schedule_id = %schedule.id,
                error = %e,
                "local_scheduler: internal.add failed",
            );
            return;
        }
    };
    st.schedules.insert(schedule.id.clone(), schedule.clone());
    st.registered.insert(schedule.id.clone(), job_uuid);
    info!(
        schedule_id = %schedule_id,
        when = %schedule.when,
        poll_cron = %cron,
        tz = ?lowered.tz,
        "local_scheduler: registered",
    );
    // A past-dated calendar one-shot never fires — warn so it's
    // diagnosable from the agent log (claude #432 review). Mirrors
    // the backend scheduler's register() check.
    if let When::Calendar(c) = &schedule.when {
        if let Some(fires_at) = c.oneshot_instant(schedule.tz) {
            if fires_at < Utc::now() {
                warn!(
                    schedule_id = %schedule_id,
                    %fires_at,
                    "local_scheduler: calendar one-shot date is in the past — it will never fire",
                );
            }
        }
    }
    // A corrupt constraints.window fails closed — warn so the stuck
    // schedule is diagnosable (gemini #452 review).
    if let Some(err) = schedule.bad_window() {
        warn!(
            schedule_id = %schedule_id,
            %err,
            "local_scheduler: constraints.window unparseable — blocked (fail-closed) until fixed",
        );
    }
}

async fn unregister_locally(internal: &JobScheduler, state: &Arc<Mutex<State>>, schedule_id: &str) {
    let uuid_opt = {
        let mut st = state.lock().await;
        st.schedules.remove(schedule_id);
        st.registered.remove(schedule_id)
    };
    if let Some(uuid) = uuid_opt {
        if let Err(e) = internal.remove(&uuid).await {
            warn!(error = %e, schedule_id, "local_scheduler: remove failed");
        } else {
            info!(schedule_id, "local_scheduler: unregistered");
        }
    }
}

async fn local_tick(
    client: &async_nats::Client,
    pc_id: &str,
    state: &Arc<Mutex<State>>,
    schedule: &Schedule,
    staleness: &crate::staleness::Tracker,
    script_cache: &ScriptCache,
) {
    // 0) Dormant outside the optional `active.{from,until}` window
    //    (#418 decision G) — mirrors the backend scheduler's gate so
    //    runs_on: agent campaigns end on the same instant.
    if !schedule.active.contains(Utc::now(), schedule.tz) {
        debug!(
            schedule_id = %schedule.id,
            "local_scheduler: outside active window (dormant)",
        );
        return;
    }

    // 0b) Maintenance window (#418 Phase 3) — same gate as the
    //     backend scheduler, evaluated in this agent's tz.
    if !schedule.constraints.allows(Utc::now(), schedule.tz) {
        debug!(
            schedule_id = %schedule.id,
            "local_scheduler: outside maintenance window — skip",
        );
        return;
    }

    // 1) Manifest + (optional) pre-resolved script_object digest
    //    must be cached. If not, skip and try again next tick (the
    //    jobs_watch loop may pick it up).
    let resolved = {
        let st = state.lock().await;
        match st.jobs.get(&schedule.job_id).cloned() {
            Some(r) => r,
            None => {
                warn!(
                    schedule_id = %schedule.id,
                    job_id = %schedule.job_id,
                    "local_scheduler: job not in cache yet — skip this tick",
                );
                return;
            }
        }
    };
    let ResolvedJob {
        manifest,
        script_object_sha256: cached_digest,
    } = resolved;

    // 2) Mode-based dedup against local_completions.
    let now = Utc::now();
    let lowered = schedule.lowered();
    // Defensive parse (gemini #419 review): validate() rejects a bad
    // `every` at create time, but a hand-edited KV blob bypasses
    // that. Silently mapping a parse failure to `None` would turn
    // the schedule into "permanent skip after first success" under
    // OncePerPc — warn + skip the tick instead, mirroring the
    // backend scheduler's parse_cooldown error path.
    let cooldown = match lowered.cooldown.as_deref() {
        None => None,
        Some(raw) => match humantime::parse_duration(raw)
            .ok()
            .and_then(|d| ChronoDuration::from_std(d).ok())
        {
            Some(cd) => Some(cd),
            None => {
                warn!(
                    schedule_id = %schedule.id,
                    every = %raw,
                    "local_scheduler: invalid when.every duration; skipping tick",
                );
                return;
            }
        },
    };
    let should_fire = match lowered.mode {
        ExecMode::EveryTick => true,
        ExecMode::OncePerTarget => {
            // per_target needs fleet-wide completion data and is
            // rejected by Schedule::validate() for runs_on: agent —
            // this branch is only reachable through a hand-edited
            // KV blob, so skip loudly instead of silently degrading
            // to per_pc like pre-#418 code did.
            warn!(
                schedule_id = %schedule.id,
                "local_scheduler: when.per_target is backend-only \
                 (validate() rejects it for runs_on: agent); skipping tick",
            );
            return;
        }
        ExecMode::OncePerPc => {
            let st = state.lock().await;
            let key = State::key(&schedule.id, &schedule.job_id);
            match st.completions.get(&key) {
                None => true,
                Some(last) => match cooldown {
                    None => false, // permanent skip after first success
                    Some(cd) => (now - *last) >= cd,
                },
            }
        }
    };
    if !should_fire {
        debug!(
            schedule_id = %schedule.id,
            "local_scheduler: dedup says skip",
        );
        return;
    }

    // 3) Build a Command in-process (no NATS hop) and call
    //    handle_command directly. Skip the deadline (= None) since
    //    we just fired this very instant — no delivery lag.
    //
    // #210 / Gemini #214 HIGH: build the Command in the same shape
    // backend's exec.rs would — inline body for `script:` manifests,
    // or (script: "", script_object: Some(key), script_object_sha256:
    // Some(cached_digest)) for `script_object:` ones. The digest was
    // pre-resolved at apply_resync / jobs_watch time, so this path
    // doesn't touch the broker — `runs_on: agent` keeps firing
    // script_object jobs during broker outages from the last
    // successful resync's cache.
    let (script_body, script_object_ref) = match (
        manifest.execute.script.as_deref().filter(|s| !s.is_empty()),
        manifest.execute.script_object.as_deref(),
        cached_digest,
    ) {
        (Some(inline), _, _) => (inline.to_owned(), None),
        (None, Some(key), Some(digest)) => (String::new(), Some((key.to_owned(), digest))),
        (None, Some(key), None) => {
            warn!(
                schedule_id = %schedule.id,
                job_id = %manifest.id,
                %key,
                "local_scheduler: script_object digest not in cache (last resync's fetch failed); \
                 skipping tick — next successful resync will populate it",
            );
            return;
        }
        (None, None, _) => {
            warn!(
                schedule_id = %schedule.id,
                job_id = %manifest.id,
                "local_scheduler: manifest has no script source — Manifest::validate() should have caught this; skipping tick",
            );
            return;
        }
    };
    let timeout_secs = humantime::parse_duration(&manifest.execute.timeout)
        .ok()
        .map(|d| d.as_secs())
        .unwrap_or(60);
    let jitter_secs = schedule
        .plan
        .jitter
        .as_deref()
        .and_then(|s| humantime::parse_duration(s).ok())
        .map(|d| d.as_secs());
    let exec_id = Uuid::new_v4().to_string();
    let cmd = Command {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        request_id: Uuid::new_v4().to_string(),
        exec_id: Some(exec_id),
        shell: manifest.execute.shell.into(),
        script: script_body,
        script_object: script_object_ref.as_ref().map(|(k, _)| k.clone()),
        script_object_sha256: script_object_ref.as_ref().map(|(_, d)| d.clone()),
        timeout_secs,
        jitter_secs,
        run_as: manifest.execute.run_as,
        cwd: manifest.execute.cwd.clone(),
        deadline_at: None,
        // v0.26: forward the Manifest's Layer 2 staleness policy so
        // `handle_command` evaluates it against the agent's current
        // broker-connectivity reading at fire time.
        staleness: manifest.staleness.clone(),
        // Issue #246: forward the manifest's observability emit hint
        // so the agent routes stdout NDJSON to obs-outbox on fire.
        // Same forward rationale as `staleness` — no manifest re-fetch.
        emit: manifest.emit.clone(),
    };

    let js = async_nats::jetstream::new(client.clone());
    let script_current = js.get_key_value(BUCKET_SCRIPT_CURRENT).await.ok();
    let script_status = js.get_key_value(BUCKET_SCRIPT_STATUS).await.ok();

    info!(
        schedule_id = %schedule.id,
        job_id = %manifest.id,
        request_id = %cmd.request_id,
        "local_scheduler: firing (runs_on: agent)",
    );

    // 4) Drive the same handle_command as the live-NATS path.
    let request_id = cmd.request_id.clone();
    let job_id_for_completion = manifest.id.clone();
    match handle_command(
        client.clone(),
        pc_id.to_string(),
        cmd,
        script_current,
        script_status,
        staleness.clone(),
        script_cache.clone(),
    )
    .await
    {
        Ok(()) => {
            // 5) Record the completion. handle_command publishes a
            //    result to NATS, but we don't know its exit_code
            //    here — accept "no error = the run finished, take
            //    that as a successful tick" for v0.23 MVP. The
            //    operator's source of truth for actual exit codes
            //    remains the Results page once results flush.
            state
                .lock()
                .await
                .record_completion(&schedule.id, &job_id_for_completion, Utc::now());
            info!(
                schedule_id = %schedule.id,
                %request_id,
                "local_scheduler: completion recorded",
            );
        }
        Err(e) => {
            warn!(
                schedule_id = %schedule.id,
                %request_id,
                error = %e,
                "local_scheduler: handle_command failed (will retry next tick)",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::manifest::{
        Active, Constraints, FanoutPlan, OnceLiteral, PerPolicy, ScheduleTz, Target, When,
    };

    fn schedule(target: Target, runs_on: RunsOn) -> Schedule {
        Schedule {
            id: "s".into(),
            when: When::PerPc(PerPolicy::Once(OnceLiteral::Once)),
            job_id: "j".into(),
            plan: FanoutPlan {
                target,
                ..Default::default()
            },
            active: Active::default(),
            constraints: Constraints::default(),
            tz: ScheduleTz::default(),
            starting_deadline: None,
            runs_on,
            enabled: true,
        }
    }

    #[test]
    fn target_all_matches_anyone() {
        let s = schedule(
            Target {
                all: true,
                ..Default::default()
            },
            RunsOn::Agent,
        );
        assert!(target_includes(&s, "pc-01", &[]));
    }

    #[test]
    fn target_pcs_explicit_match() {
        let s = schedule(
            Target {
                pcs: vec!["pc-01".into()],
                ..Default::default()
            },
            RunsOn::Agent,
        );
        assert!(target_includes(&s, "pc-01", &[]));
        assert!(!target_includes(&s, "other", &[]));
    }

    #[test]
    fn target_groups_intersect() {
        let s = schedule(
            Target {
                groups: vec!["canary".into(), "wave1".into()],
                ..Default::default()
            },
            RunsOn::Agent,
        );
        assert!(target_includes(&s, "any", &["wave1".into()]));
        assert!(target_includes(
            &s,
            "any",
            &["dept-eng".into(), "canary".into()]
        ));
        assert!(!target_includes(&s, "any", &["dept-eng".into()]));
    }

    #[test]
    fn target_none_matches_none() {
        let s = schedule(Target::default(), RunsOn::Agent);
        assert!(!target_includes(&s, "pc-01", &["canary".into()]));
    }
}
