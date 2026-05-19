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
use kanade_shared::manifest::{ExecMode, Manifest, RunsOn, Schedule};
use kanade_shared::wire::Command;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::commands::handle_command;

/// In-memory state shared across the watch loops and the tick
/// callbacks. Wrapped in a single `Mutex<State>` because the scheduler
/// only ticks one job at a time and the watch loops are also serial.
struct State {
    /// Latest snapshot of every job in BUCKET_JOBS.
    jobs: HashMap<String, Manifest>,
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(client, pc_id, completions_path, groups_rx, staleness).await {
            error!(error = ?e, "local_scheduler loop exited with error");
        }
    })
}

async fn run(
    client: async_nats::Client,
    pc_id: String,
    completions_path: PathBuf,
    groups_rx: tokio::sync::watch::Receiver<Vec<String>>,
    staleness: crate::staleness::Tracker,
) -> Result<()> {
    let js = async_nats::jetstream::new(client.clone());
    let schedules_kv = js
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .with_context(|| format!("get KV {BUCKET_SCHEDULES}"))?;
    let jobs_kv = js
        .get_key_value(BUCKET_JOBS)
        .await
        .with_context(|| format!("get KV {BUCKET_JOBS}"))?;

    let internal = JobScheduler::new()
        .await
        .context("init internal JobScheduler")?;
    internal
        .start()
        .await
        .context("start internal JobScheduler")?;

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

    // 1) Prime the job cache so initial-load schedule registrations
    //    can find their Manifest.
    prime_jobs_cache(&jobs_kv, &state).await;

    // 2) Initial group membership snapshot from the watch channel
    //    (`groups::spawn` publishes the current value the moment the
    //    KV read finishes, so this borrow is safe even before any
    //    edit fires).
    let my_groups = groups_rx.borrow().clone();
    info!(
        pc_id = %pc_id,
        groups = ?my_groups,
        "local_scheduler: initial group membership snapshot",
    );

    // 3) Initial pass over schedules KV.
    if let Ok(keys) = schedules_kv.keys().await {
        let keys: Vec<String> = keys.try_collect().await.unwrap_or_default();
        for k in keys {
            if let Ok(Some(bytes)) = schedules_kv.get(&k).await
                && let Ok(s) = serde_json::from_slice::<Schedule>(&bytes)
            {
                reconcile_schedule(
                    &internal, &state, &client, &pc_id, &my_groups, &s, &staleness,
                )
                .await;
            }
        }
    }
    let count = state.lock().await.registered.len();
    info!(count, "local_scheduler: registered initial schedules");

    // 4) Three concurrent watch loops:
    //   * schedules KV → reconcile / unregister per entry
    //   * jobs KV       → keep the Manifest cache fresh
    //   * groups_rx     → membership changes → re-reconcile ALL
    //                     cached schedules so group-targeted ones
    //                     get picked up / dropped right away
    let schedules_watch = schedules_kv
        .watch_all()
        .await
        .context("schedules KV watch_all")?;
    let jobs_watch = jobs_kv.watch_all().await.context("jobs KV watch_all")?;

    let internal_for_sched = internal.clone();
    let state_for_sched = state.clone();
    let client_for_sched = client.clone();
    let pc_id_for_sched = pc_id.clone();
    let groups_rx_for_sched = groups_rx.clone();
    let staleness_for_sched = staleness.clone();
    let sched_task = tokio::spawn(async move {
        let mut watch = schedules_watch;
        while let Some(entry) = watch.next().await {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "schedules watch error");
                    continue;
                }
            };
            let groups_snapshot = groups_rx_for_sched.borrow().clone();
            match entry.operation {
                Operation::Put => {
                    if let Ok(s) = serde_json::from_slice::<Schedule>(&entry.value) {
                        reconcile_schedule(
                            &internal_for_sched,
                            &state_for_sched,
                            &client_for_sched,
                            &pc_id_for_sched,
                            &groups_snapshot,
                            &s,
                            &staleness_for_sched,
                        )
                        .await;
                    } else {
                        warn!(key = %entry.key, "deserialize Schedule on watch");
                    }
                }
                Operation::Delete | Operation::Purge => {
                    unregister_locally(&internal_for_sched, &state_for_sched, &entry.key).await;
                }
            }
        }
    });

    let state_for_jobs = state.clone();
    let jobs_task = tokio::spawn(async move {
        let mut watch = jobs_watch;
        while let Some(entry) = watch.next().await {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "jobs watch error");
                    continue;
                }
            };
            let mut s = state_for_jobs.lock().await;
            match entry.operation {
                Operation::Put => {
                    if let Ok(m) = serde_json::from_slice::<Manifest>(&entry.value) {
                        s.jobs.insert(entry.key.clone(), m);
                        debug!(job_id = %entry.key, "local_scheduler: cached job manifest");
                    }
                }
                Operation::Delete | Operation::Purge => {
                    s.jobs.remove(&entry.key);
                }
            }
        }
    });

    // v0.24: group-membership change handler. Re-reconciles every
    // schedule the agent already knows about so target.groups overlap
    // re-evaluates without waiting for the next schedule edit.
    let internal_for_groups = internal.clone();
    let state_for_groups = state.clone();
    let client_for_groups = client.clone();
    let pc_id_for_groups = pc_id.clone();
    let staleness_for_groups = staleness.clone();
    let mut groups_rx_for_watch = groups_rx;
    let groups_task = tokio::spawn(async move {
        // Skip the initial value — already used above for the boot
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
            let cached: Vec<Schedule> = {
                let st = state_for_groups.lock().await;
                st.schedules.values().cloned().collect()
            };
            // Also need to consider schedules that AREN'T currently
            // registered (= weren't ours before but might be now).
            // Walk schedules KV again.
            let js = async_nats::jetstream::new(client_for_groups.clone());
            let kv = match js.get_key_value(BUCKET_SCHEDULES).await {
                Ok(k) => k,
                Err(e) => {
                    warn!(error = %e, "groups change: schedules KV unavailable");
                    continue;
                }
            };
            let keys: Vec<String> = match kv.keys().await {
                Ok(s) => s.try_collect().await.unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let mut seen = std::collections::HashSet::new();
            for k in keys {
                seen.insert(k.clone());
                if let Ok(Some(bytes)) = kv.get(&k).await
                    && let Ok(s) = serde_json::from_slice::<Schedule>(&bytes)
                {
                    reconcile_schedule(
                        &internal_for_groups,
                        &state_for_groups,
                        &client_for_groups,
                        &pc_id_for_groups,
                        &new_groups,
                        &s,
                        &staleness_for_groups,
                    )
                    .await;
                }
            }
            // Drop schedules that disappeared from KV between
            // the schedule-watch pass and now.
            for cached_s in cached {
                if !seen.contains(&cached_s.id) {
                    unregister_locally(&internal_for_groups, &state_for_groups, &cached_s.id).await;
                }
            }
        }
    });

    let _ = tokio::join!(sched_task, jobs_task, groups_task);
    Ok(())
}

async fn prime_jobs_cache(jobs_kv: &async_nats::jetstream::kv::Store, state: &Arc<Mutex<State>>) {
    let Ok(keys) = jobs_kv.keys().await else {
        return;
    };
    let keys: Vec<String> = keys.try_collect().await.unwrap_or_default();
    for k in keys {
        if let Ok(Some(bytes)) = jobs_kv.get(&k).await
            && let Ok(m) = serde_json::from_slice::<Manifest>(&bytes)
        {
            state.lock().await.jobs.insert(k, m);
        }
    }
}

// v0.24: `read_my_groups` removed — membership now flows through the
// `groups::spawn` watch channel that `local_scheduler` subscribes to,
// so we no longer poll the KV ourselves.

async fn reconcile_schedule(
    internal: &JobScheduler,
    state: &Arc<Mutex<State>>,
    client: &async_nats::Client,
    pc_id: &str,
    my_groups: &[String],
    schedule: &Schedule,
    staleness: &crate::staleness::Tracker,
) {
    let mine = {
        let st = state.lock().await;
        st.matching(schedule, pc_id, my_groups)
    };

    // Always unregister an existing copy first — cron / target /
    // enabled edits all need to land.
    unregister_locally(internal, state, &schedule.id).await;

    if !mine {
        return;
    }

    let cron = schedule.cron.clone();
    let schedule_id = schedule.id.clone();
    let client_for_job = client.clone();
    let pc_id_for_job = pc_id.to_string();
    let state_for_job = state.clone();
    let schedule_for_job = schedule.clone();
    let staleness_for_job = staleness.clone();
    let job = match Job::new_async(cron.as_str(), move |_uuid, _l| {
        let client = client_for_job.clone();
        let pc_id = pc_id_for_job.clone();
        let state = state_for_job.clone();
        let schedule = schedule_for_job.clone();
        let staleness = staleness_for_job.clone();
        Box::pin(async move {
            local_tick(&client, &pc_id, &state, &schedule, &staleness).await;
        })
    }) {
        Ok(j) => j,
        Err(e) => {
            warn!(
                schedule_id = %schedule.id,
                error = %e,
                "local_scheduler: Job::new_async failed",
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
    {
        let mut st = state.lock().await;
        st.schedules.insert(schedule.id.clone(), schedule.clone());
        st.registered.insert(schedule.id.clone(), job_uuid);
    }
    info!(
        schedule_id = %schedule_id,
        cron = %cron,
        mode = ?schedule.mode,
        "local_scheduler: registered",
    );
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
) {
    // 1) Manifest must be cached. If not, skip and try again next tick
    //    (the jobs_watch loop may pick it up).
    let manifest = {
        let st = state.lock().await;
        match st.jobs.get(&schedule.job_id).cloned() {
            Some(m) => m,
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

    // 2) Mode-based dedup against local_completions.
    let now = Utc::now();
    let cooldown = schedule
        .cooldown
        .as_deref()
        .and_then(|s| humantime::parse_duration(s).ok())
        .and_then(|d| ChronoDuration::from_std(d).ok());
    let should_fire = match schedule.mode {
        ExecMode::EveryTick => true,
        ExecMode::OncePerPc | ExecMode::OncePerTarget => {
            // OncePerTarget is meaningless when each agent is
            // self-scheduled (we'd be deduping across a target of 1).
            // Treat as OncePerPc and warn once at register time would
            // be cleaner; for now just behave-as-OncePerPc silently.
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
        job_id: Some(exec_id),
        shell: manifest.execute.shell.into(),
        script: manifest.execute.script.clone(),
        timeout_secs,
        jitter_secs,
        run_as: manifest.execute.run_as,
        cwd: manifest.execute.cwd.clone(),
        deadline_at: None,
        // v0.26: forward the Manifest's Layer 2 staleness policy so
        // `handle_command` evaluates it against the agent's current
        // broker-connectivity reading at fire time.
        staleness: manifest.staleness.clone(),
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
    use kanade_shared::manifest::{FanoutPlan, Target};

    fn schedule(target: Target, runs_on: RunsOn) -> Schedule {
        Schedule {
            id: "s".into(),
            cron: "* * * * * *".into(),
            job_id: "j".into(),
            plan: FanoutPlan {
                target,
                ..Default::default()
            },
            mode: ExecMode::EveryTick,
            cooldown: None,
            auto_disable_when_done: false,
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
        assert!(target_includes(&s, "minipc", &[]));
    }

    #[test]
    fn target_pcs_explicit_match() {
        let s = schedule(
            Target {
                pcs: vec!["minipc".into()],
                ..Default::default()
            },
            RunsOn::Agent,
        );
        assert!(target_includes(&s, "minipc", &[]));
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
        assert!(!target_includes(&s, "minipc", &["canary".into()]));
    }
}
