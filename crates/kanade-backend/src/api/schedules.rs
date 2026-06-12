use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use futures::TryStreamExt;
use kanade_shared::kv::{
    BUCKET_SCHEDULES, BUCKET_SCHEDULES_YAML, BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_REVOKED,
};
use kanade_shared::manifest::{RunsOn, Schedule};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

use crate::api::AppState;
use crate::api::yaml_body::{YamlOrJson, mirror_yaml, yaml_headers};
use crate::audit;
use crate::audit::Caller;

#[derive(Serialize)]
pub struct ScheduleSummary {
    pub id: String,
    /// Operator-facing one-liner (`When`'s Display): `per_pc once`,
    /// `per_pc every 6h`, `cron: 0 0 9 * * mon-fri`, …
    pub when: String,
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

/// Query params for [`preview`].
#[derive(Deserialize, Debug)]
pub struct PreviewQuery {
    /// How many upcoming fires to list (calendar schedules only).
    /// Defaults to 5; clamped to `1..=50` so a huge `count` can't make
    /// the backend walk croner thousands of times per request.
    #[serde(default = "default_preview_count")]
    pub count: usize,
}

fn default_preview_count() -> usize {
    5
}

/// Dry-run result for [`preview`].
#[derive(Serialize)]
pub struct PreviewResponse {
    pub id: String,
    /// `When`'s Display — `at 09:00 [mon-fri]`, `per_pc every 6h`, …
    pub when: String,
    /// `local` / `utc` — the tz the fire times are resolved in.
    pub tz: String,
    /// The schedule's `enabled` flag. The fire times are computed from
    /// the cron regardless, so a disabled schedule still previews its
    /// *would-be* fires — surface the flag so callers don't mistake a
    /// dormant schedule for an active one (claude #578 review).
    pub enabled: bool,
    /// Upcoming fire instants (RFC3339 UTC), soonest first. Empty for
    /// reconcile shapes and for calendars that can never fire — see
    /// `note`.
    pub fires: Vec<String>,
    /// Present only when `fires` is empty, explaining why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// GET /api/schedules/{id}/preview?count=N — dry-run the next N fire
/// times of a schedule (#418 "ドライラン / プレビュー"). Calendar
/// schedules return discrete tz-resolved instants (honoring the
/// `active` window + `constraints.window`); reconcile shapes have no
/// discrete fire times, so `fires` is empty and `note` describes the
/// cadence. Read-only: never touches KV state.
pub async fn preview(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<PreviewResponse>, (StatusCode, String)> {
    let kv = s
        .jetstream
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("schedules bucket missing: {e}"),
            )
        })?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("schedule '{id}' not found")))?;
    let schedule: Schedule = serde_json::from_slice(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("deserialize stored schedule: {e}"),
        )
    })?;

    let count = q.count.clamp(1, 50);
    let fires = schedule.preview_fires(chrono::Utc::now(), count);
    let note = if !fires.is_empty() {
        None
    } else if matches!(schedule.when, kanade_shared::manifest::When::Calendar(_)) {
        Some(
            "no upcoming fires — a past one-shot, or the fire time is excluded by the \
             active window / constraints.window"
                .to_string(),
        )
    } else {
        Some(format!(
            "reconcile cadence ({}) polls every minute gated by cooldown — no discrete \
             fire times to preview",
            schedule.when
        ))
    };

    Ok(Json(PreviewResponse {
        id: schedule.id.clone(),
        when: schedule.when.to_string(),
        tz: schedule.tz.as_str().to_string(),
        enabled: schedule.enabled,
        fires: fires.iter().map(|t| t.to_rfc3339()).collect(),
        note,
    }))
}

/// Trailing window for the success/fail tally in [`status`].
const STATUS_WINDOW_HOURS: i64 = 24;

/// The most recent run of a schedule's job (`exit_code` / `finished_at`
/// `null` = still in flight).
#[derive(Serialize, Default)]
pub struct LastRun {
    pub pc_id: String,
    pub exit_code: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// Finished-run tally over the trailing [`STATUS_WINDOW_HOURS`].
#[derive(Serialize, Default)]
pub struct RecentCounts {
    pub window_hours: i64,
    pub ok: i64,
    pub fail: i64,
}

/// Coverage view for [`status`].
#[derive(Serialize)]
pub struct StatusResponse {
    pub id: String,
    pub when: String,
    pub tz: String,
    pub enabled: bool,
    /// Soonest upcoming fire (RFC3339 UTC) — calendar schedules only;
    /// `null` for reconcile shapes or a schedule that can never fire.
    pub next_run: Option<String>,
    /// Most recent run, or `null` if this schedule's job has never run.
    pub last_run: Option<LastRun>,
    pub recent: RecentCounts,
}

/// Last run + trailing success/fail tally for a job. Keyed by `job_id`
/// (a schedule references a job; `execution_results` has no
/// `schedule_id`), so two schedules sharing a job share these numbers —
/// accurate for the common 1:1 case, an over-count otherwise. Pure DB
/// read, factored out so it's unit-testable against an in-memory pool.
async fn schedule_run_stats(
    pool: &sqlx::SqlitePool,
    job_id: &str,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<(Option<LastRun>, RecentCounts), sqlx::Error> {
    use sqlx::Row;
    let last = sqlx::query(
        "SELECT pc_id, exit_code, started_at, finished_at
           FROM execution_results
          WHERE job_id = ?
          ORDER BY recorded_at DESC
          LIMIT 1",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .map(|r| LastRun {
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        // Read the nullable columns as `Option<_>` explicitly:
        // sqlx-sqlite decodes a NULL via `try_get::<i64>` / `<String>`
        // into `0` / `""` rather than erroring, so `try_get(..).ok()`
        // would mislabel a still-running row (NULL exit_code +
        // finished_at) as "exit 0, finished at ''". The Option form maps
        // NULL → None. `started_at` is NOT NULL so it stays a plain read.
        exit_code: r.try_get::<Option<i64>, _>("exit_code").unwrap_or(None),
        started_at: r.try_get("started_at").ok(),
        finished_at: r
            .try_get::<Option<String>, _>("finished_at")
            .unwrap_or(None),
    });
    let counts = sqlx::query(
        "SELECT
             COALESCE(SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END), 0) AS ok,
             COALESCE(SUM(CASE WHEN exit_code IS NOT NULL AND exit_code <> 0 THEN 1 ELSE 0 END), 0) AS fail
           FROM execution_results
          WHERE job_id = ? AND finished_at IS NOT NULL AND recorded_at >= ?",
    )
    .bind(job_id)
    .bind(since)
    .fetch_one(pool)
    .await?;
    let recent = RecentCounts {
        window_hours: STATUS_WINDOW_HOURS,
        ok: counts.try_get("ok").unwrap_or(0),
        fail: counts.try_get("fail").unwrap_or(0),
    };
    Ok((last, recent))
}

/// GET /api/schedules/{id}/status — coverage view: enabled, next fire
/// (via `preview_fires`), the schedule's most recent run, and a 24h
/// ok/fail tally (#418 "カバレッジ可視化"). Read-only. The run figures
/// are `job_id`-keyed — see [`schedule_run_stats`].
pub async fn status(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let kv = s
        .jetstream
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("schedules bucket missing: {e}"),
            )
        })?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("schedule '{id}' not found")))?;
    let schedule: Schedule = serde_json::from_slice(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("deserialize stored schedule: {e}"),
        )
    })?;

    let now = chrono::Utc::now();
    let next_run = schedule
        .preview_fires(now, 1)
        .first()
        .map(chrono::DateTime::to_rfc3339);
    let since = now - chrono::Duration::hours(STATUS_WINDOW_HOURS);
    let (last_run, recent) = schedule_run_stats(&s.pool, &schedule.job_id, since)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("run stats: {e}")))?;

    Ok(Json(StatusResponse {
        id: schedule.id.clone(),
        when: schedule.when.to_string(),
        tz: schedule.tz.as_str().to_string(),
        enabled: schedule.enabled,
        next_run,
        last_run,
        recent,
    }))
}

// ---- #418 rollout coverage (N targeted, M completed) ----

/// One agent's standing in a schedule's rollout.
#[derive(Serialize)]
pub struct AgentRun {
    pub pc_id: String,
    /// `"ok"` | `"fail"` | `"running"` | `"pending"`.
    pub state: &'static str,
    /// Manifest version pinned on the agent's latest finished run —
    /// the lever for vuln-response version tracking. `null` while
    /// running / pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// RFC3339 finish time of that run; `null` while running / pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

/// Rollout coverage of a schedule across its FULL targeted roster
/// (offline hosts included → `pending`). `job_id`-keyed like
/// [`schedule_run_stats`]: any run of the job counts, which is the
/// right "did this upgrade reach the host" signal but means schedules
/// sharing a job (or ad-hoc execs of it) share the figures.
#[derive(Serialize, Default)]
pub struct CoverageResponse {
    pub id: String,
    pub when: String,
    pub job_id: String,
    pub runs_on: String,
    /// Size of the full targeted roster — the "N" of "M of N".
    pub total: usize,
    pub ok: usize,
    pub fail: usize,
    pub running: usize,
    pub pending: usize,
    pub agents: Vec<AgentRun>,
}

/// Per-schedule coverage counts for the list view (no per-agent detail).
#[derive(Serialize)]
pub struct CoverageSummary {
    pub id: String,
    pub total: usize,
    pub ok: usize,
    pub fail: usize,
    pub running: usize,
    pub pending: usize,
}

fn runs_on_str(r: RunsOn) -> &'static str {
    match r {
        RunsOn::Backend => "backend",
        RunsOn::Agent => "agent",
    }
}

/// An agent's latest finished run for a job: `(exit_code, version,
/// finished_at)`. NULL-able since a finished row can carry a NULL exit.
type FinishedRun = (Option<i64>, Option<String>, Option<String>);
/// pc_id → its latest finished run, for one job.
type FinishedMap = HashMap<String, FinishedRun>;

/// Pure rollout-coverage aggregation, factored out of [`coverage`] so
/// it's unit-testable without a DB/KV. For each pc in the FULL roster:
/// in-flight (a row with `finished_at IS NULL`) → running; else its
/// latest finished run's exit code → ok (`0`) / fail (anything else,
/// incl. a NULL exit on a finished row); else (no rows at all) →
/// pending. Offline-but-targeted hosts have no rows ⇒ pending — exactly
/// the "hasn't rolled out yet" signal. Returns `(agents, ok, fail,
/// running, pending)`.
fn coverage_for(
    roster: &[String],
    inflight: &HashSet<String>,
    finished: &FinishedMap,
) -> (Vec<AgentRun>, usize, usize, usize, usize) {
    let mut agents = Vec::with_capacity(roster.len());
    let (mut ok, mut fail, mut running, mut pending) = (0usize, 0usize, 0usize, 0usize);
    for pc in roster {
        if inflight.contains(pc) {
            running += 1;
            agents.push(AgentRun {
                pc_id: pc.clone(),
                state: "running",
                version: None,
                finished_at: None,
            });
        } else if let Some((exit, version, finished_at)) = finished.get(pc) {
            let state = match exit {
                Some(0) => {
                    ok += 1;
                    "ok"
                }
                _ => {
                    fail += 1;
                    "fail"
                }
            };
            agents.push(AgentRun {
                pc_id: pc.clone(),
                state,
                version: version.clone(),
                finished_at: finished_at.clone(),
            });
        } else {
            pending += 1;
            agents.push(AgentRun {
                pc_id: pc.clone(),
                state: "pending",
                version: None,
                finished_at: None,
            });
        }
    }
    (agents, ok, fail, running, pending)
}

/// Per-job in-flight set + latest-finished map for one job. The latest
/// row per pc is picked deterministically via `ROW_NUMBER()` ordered by
/// `finished_at DESC, result_id DESC` — so a same-millisecond tie
/// resolves stably (claude #617) instead of relying on JOIN row order.
/// Excludes stdout/stderr.
async fn coverage_rows(
    pool: &sqlx::SqlitePool,
    job_id: &str,
) -> Result<(HashSet<String>, FinishedMap), sqlx::Error> {
    use sqlx::Row;
    let inflight: HashSet<String> = sqlx::query(
        "SELECT DISTINCT pc_id FROM execution_results
          WHERE job_id = ? AND finished_at IS NULL",
    )
    .bind(job_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .filter_map(|r| r.try_get::<String, _>("pc_id").ok())
    .collect();

    let mut finished = HashMap::new();
    let rows = sqlx::query(
        "SELECT pc_id, exit_code, finished_at, version FROM (
             SELECT pc_id, exit_code, finished_at, version,
                    ROW_NUMBER() OVER (
                        PARTITION BY pc_id
                        ORDER BY finished_at DESC, result_id DESC
                    ) AS rn
               FROM execution_results
              WHERE job_id = ? AND finished_at IS NOT NULL
         ) WHERE rn = 1",
    )
    .bind(job_id)
    .fetch_all(pool)
    .await?;
    for r in rows {
        let pc: String = r.try_get("pc_id").unwrap_or_default();
        if pc.is_empty() {
            continue;
        }
        // NULL-safe reads (see schedule_run_stats for the sqlx-sqlite
        // NULL-decodes-to-default quirk).
        let exit = r.try_get::<Option<i64>, _>("exit_code").unwrap_or(None);
        let version = r.try_get::<Option<String>, _>("version").unwrap_or(None);
        let finished_at = r
            .try_get::<Option<String>, _>("finished_at")
            .unwrap_or(None);
        finished.insert(pc, (exit, version, finished_at));
    }
    Ok((inflight, finished))
}

/// GET /api/schedules/{id}/coverage — rollout coverage for one
/// schedule: how many of its FULL targeted roster have completed-ok /
/// failed / are running / are still pending, with the manifest version
/// each agent last ran (#418 "ロールアウト・カバレッジ可視化"). The
/// roster includes offline hosts so "pending" reflects true rollout
/// progress. Read-only.
pub async fn coverage(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CoverageResponse>, (StatusCode, String)> {
    let kv = s
        .jetstream
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("schedules bucket missing: {e}"),
            )
        })?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("schedule '{id}' not found")))?;
    let schedule: Schedule = serde_json::from_slice(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("deserialize stored schedule: {e}"),
        )
    })?;

    let roster = crate::scheduler::resolve_roster(&s, &schedule.plan.target, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("roster: {e}")))?;
    let (inflight, finished) = coverage_rows(&s.pool, &schedule.job_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("coverage: {e}")))?;
    let (agents, ok, fail, running, pending) = coverage_for(&roster, &inflight, &finished);

    Ok(Json(CoverageResponse {
        id: schedule.id.clone(),
        when: schedule.when.to_string(),
        job_id: schedule.job_id.clone(),
        runs_on: runs_on_str(schedule.runs_on).to_string(),
        total: roster.len(),
        ok,
        fail,
        running,
        pending,
        agents,
    }))
}

/// GET /api/schedules/coverage — coverage counts for EVERY schedule in
/// one shot (list-view progress bars, no per-agent detail).
///
/// Scaling: the `execution_results` row scan is **2 queries total**
/// (one in-flight, one latest-finished, keyed by the union of job_ids).
/// Rosters are resolved once per **distinct target** (deduped — many
/// schedules share `all: true`) and **concurrently**, so the
/// agents-table / KV work is O(distinct targets), not O(schedules).
/// Caveat: a `groups` target with `alive_only = false` still reads the
/// group KV once per agent ever seen; at thousands-of-PCs scale that
/// wants an `agent_groups` SQL projection (follow-up, gemini #617).
/// Read-only.
pub async fn coverage_summary(
    State(s): State<AppState>,
) -> Result<Json<Vec<CoverageSummary>>, (StatusCode, String)> {
    use futures::StreamExt;
    use sqlx::Row;
    let kv = match s.jetstream.get_key_value(BUCKET_SCHEDULES).await {
        Ok(k) => k,
        Err(_) => return Ok(Json(Vec::new())),
    };
    let keys: Vec<String> = kv
        .keys()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?
        .try_collect()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?;

    // Concurrent KV fetch (gemini #617) — sequential get-per-key adds up.
    let schedules: Vec<Schedule> = futures::stream::iter(keys)
        .map(|k| {
            let kv = kv.clone();
            async move {
                kv.get(&k)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|bytes| serde_json::from_slice::<Schedule>(&bytes).ok())
            }
        })
        .buffer_unordered(16)
        .filter_map(|s| async move { s })
        .collect()
        .await;

    // Union of job_ids → two batch queries (dynamic IN list).
    let job_ids: Vec<String> = schedules
        .iter()
        .map(|s| s.job_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // job_id → in-flight pc set.
    let mut inflight: HashMap<String, HashSet<String>> = HashMap::new();
    // job_id → (pc → (exit, version, finished_at)).
    let mut finished: HashMap<String, FinishedMap> = HashMap::new();

    if !job_ids.is_empty() {
        let placeholders = vec!["?"; job_ids.len()].join(",");

        let inflight_sql = format!(
            "SELECT DISTINCT job_id, pc_id FROM execution_results
              WHERE job_id IN ({placeholders}) AND finished_at IS NULL"
        );
        // Safe: `placeholders` is a fixed count of literal `?`, no user
        // data interpolated — values go through `bind` below.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(inflight_sql));
        for jid in &job_ids {
            q = q.bind(jid);
        }
        for r in q
            .fetch_all(&s.pool)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("inflight: {e}")))?
        {
            let (Ok(jid), Ok(pc)) = (
                r.try_get::<String, _>("job_id"),
                r.try_get::<String, _>("pc_id"),
            ) else {
                continue;
            };
            inflight.entry(jid).or_default().insert(pc);
        }

        // Deterministic latest-per-(job,pc) via ROW_NUMBER (ties broken
        // by result_id, claude #617). The `job_id IN (..)` lives in the
        // inner WHERE so the planner can seek a (job_id, finished_at)
        // index instead of scanning the whole table.
        let finished_sql = format!(
            "SELECT job_id, pc_id, exit_code, finished_at, version FROM (
                 SELECT job_id, pc_id, exit_code, finished_at, version,
                        ROW_NUMBER() OVER (
                            PARTITION BY job_id, pc_id
                            ORDER BY finished_at DESC, result_id DESC
                        ) AS rn
                   FROM execution_results
                  WHERE job_id IN ({placeholders}) AND finished_at IS NOT NULL
             ) WHERE rn = 1"
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(finished_sql));
        for jid in &job_ids {
            q = q.bind(jid);
        }
        for r in q
            .fetch_all(&s.pool)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("finished: {e}")))?
        {
            let (Ok(jid), Ok(pc)) = (
                r.try_get::<String, _>("job_id"),
                r.try_get::<String, _>("pc_id"),
            ) else {
                continue;
            };
            if pc.is_empty() {
                continue;
            }
            let exit = r.try_get::<Option<i64>, _>("exit_code").unwrap_or(None);
            let version = r.try_get::<Option<String>, _>("version").unwrap_or(None);
            let finished_at = r
                .try_get::<Option<String>, _>("finished_at")
                .unwrap_or(None);
            finished
                .entry(jid)
                .or_default()
                .insert(pc, (exit, version, finished_at));
        }
    }

    // Resolve each DISTINCT target once, concurrently. The JSON form of
    // `Target` is the dedup key; many schedules share `all: true` so
    // this collapses the agents/KV work to O(distinct targets).
    let mut distinct: HashMap<String, kanade_shared::manifest::Target> = HashMap::new();
    for sched in &schedules {
        let key = serde_json::to_string(&sched.plan.target).unwrap_or_default();
        distinct
            .entry(key)
            .or_insert_with(|| sched.plan.target.clone());
    }
    let rosters: HashMap<String, Vec<String>> = futures::stream::iter(distinct)
        .map(|(key, target)| {
            let s = s.clone();
            async move {
                let roster = crate::scheduler::resolve_roster(&s, &target, false).await;
                (key, roster)
            }
        })
        .buffer_unordered(8)
        .filter_map(|(k, r)| async move { r.ok().map(|v| (k, v)) })
        .collect()
        .await;

    let empty_set: HashSet<String> = HashSet::new();
    let empty_map: FinishedMap = HashMap::new();
    let out: Vec<CoverageSummary> = schedules
        .iter()
        .map(|sched| {
            let key = serde_json::to_string(&sched.plan.target).unwrap_or_default();
            // A target that failed to resolve degrades to an empty
            // roster (0/0) rather than failing the whole list.
            let roster = rosters.get(&key).cloned().unwrap_or_default();
            let inf = inflight.get(&sched.job_id).unwrap_or(&empty_set);
            let fin = finished.get(&sched.job_id).unwrap_or(&empty_map);
            let (_, ok, fail, running, pending) = coverage_for(&roster, inf, fin);
            CoverageSummary {
                id: sched.id.clone(),
                total: roster.len(),
                ok,
                fail,
                running,
                pending,
            }
        })
        .collect();
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

    // #418 decision F: reject broken schedules at create time
    // instead of letting them sit in KV and warn-skip every tick.
    // validate() covers the pure cross-field rules; the job_id
    // existence check needs the JOBS KV, so it lives here.
    if let Err(e) = schedule.validate() {
        return Err((StatusCode::BAD_REQUEST, format!("invalid schedule: {e}")));
    }
    match crate::api::jobs::fetch(&s.jetstream, &schedule.job_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown job_id '{}' — register the job first (kanade job create)",
                    schedule.job_id
                ),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("job catalog lookup: {e}"),
            ));
        }
    }

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
        when = %schedule.when,
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
            "when": schedule.when.to_string(),
            "job_id": schedule.job_id,
            "enabled": schedule.enabled,
        }),
    )
    .await;
    Ok(Json(ScheduleSummary {
        id: schedule.id.clone(),
        when: schedule.when.to_string(),
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

/// Patch the top-level `enabled:` line in an operator's YAML source,
/// preserving every other line — comments and block-scalar formatting
/// are the reason the YAML mirror exists, so a full
/// `serde_yaml::to_string` re-render is not an option here. Column-0
/// match only, so an indented `enabled:` inside a nested map is left
/// alone. Appends the line when missing (legacy YAML that relied on
/// the serde default).
fn patch_yaml_enabled(yaml: &str, enabled: bool) -> String {
    let mut out = String::with_capacity(yaml.len() + 16);
    let mut found = false;
    for line in yaml.lines() {
        if !found && line.starts_with("enabled:") {
            // Replace the whole line. An inline comment here (e.g.
            // "# stopped during incident") describes the old value,
            // so dropping it alongside the flip is the lesser evil.
            out.push_str(&format!("enabled: {enabled}\n"));
            found = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !found {
        out.push_str(&format!("enabled: {enabled}\n"));
    }
    out
}

/// Best-effort sync of the YAML mirror after an enable/disable flip.
/// Without this, the SPA editor (which prefers `BUCKET_SCHEDULES_YAML`
/// over the JSON catalog, see [`get_yaml`]) would load the stale
/// `enabled` state and silently clobber the flip back on the next
/// save (gemini #400 review). Missing bucket / missing entry are
/// fine — `get_yaml` then falls back to rendering the (correct) JSON
/// catalog row. Write failures are warn-logged, same contract as
/// `mirror_yaml`: the JSON catalog the scheduler reads is already
/// current.
async fn sync_yaml_mirror_enabled(s: &AppState, id: &str, enabled: bool) {
    let Ok(kv) = s.jetstream.get_key_value(BUCKET_SCHEDULES_YAML).await else {
        return;
    };
    let Ok(Some(bytes)) = kv.get(id).await else {
        return;
    };
    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
        warn!(schedule_id = %id, "YAML mirror is not UTF-8; skipping enabled sync");
        return;
    };
    let patched = patch_yaml_enabled(&text, enabled);
    if let Err(e) = kv.put(id, patched.into_bytes().into()).await {
        warn!(
            error = %e,
            schedule_id = %id,
            enabled,
            "YAML mirror enabled-flag sync failed; JSON catalog is current",
        );
    }
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
    /// When `true`, also Layer 3 cascade-KILL — publish `kill.{exec_id}`
    /// for every still-running exec of `schedule.job_id` so currently-
    /// executing child processes are terminated now (SPEC §2.6.4 (c)).
    /// Orthogonal to `cascade`: kill stops *running* work, revoke stops
    /// *queued/future* work — combine both for a full hard-disable.
    /// Online-only (a kill can't reach an offline agent's child).
    /// Default `false` — killing in-flight work is a deliberate,
    /// destructive opt-in.
    #[serde(default)]
    pub cascade_kill: bool,
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
        sync_yaml_mirror_enabled(&s, &id, false).await;
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

    // Cascade Layer 3: kill currently-running children of this
    // schedule's job (SPEC §2.6.4 (c)). Enumerate the still-in-flight
    // execs (a `finished_at IS NULL` row in `execution_results` for
    // this `job_id`) and publish `kill.{exec_id}` for each — the agent
    // that's running it terminates the child via
    // `run_command_with_kill`. Orthogonal to the revoke above: this
    // stops *running* work, revoke stops *queued/future* work. Best-
    // effort + online-only: a kill can't reach an offline agent's
    // child, and a DB hiccup degrades to "no kills" (warn) rather than
    // failing the disable, which already took effect on the KV above.
    let killed_execs = if q.cascade_kill {
        match sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT exec_id FROM execution_results \
             WHERE job_id = ? AND finished_at IS NULL AND exec_id IS NOT NULL",
        )
        .bind(&schedule.job_id)
        .fetch_all(&s.pool)
        .await
        {
            Ok(exec_ids) => {
                // Publish the kills concurrently rather than awaiting
                // each in series (gemini #480) — with many in-flight
                // execs the per-publish round-trips would otherwise add
                // up. Each publish is independent; failures are logged,
                // never abort the others.
                futures::future::join_all(exec_ids.iter().map(|eid| {
                    let nats = s.nats.clone();
                    let eid = eid.clone();
                    async move {
                        if let Err(e) = nats
                            .publish(kanade_shared::subject::kill(&eid), bytes::Bytes::new())
                            .await
                        {
                            warn!(error = %e, exec_id = %eid, "schedule_disable cascade-kill: publish failed");
                        }
                    }
                }))
                .await;
                // Flush so the kills actually leave before we return —
                // otherwise a fast caller could disconnect first.
                if let Err(e) = s.nats.flush().await {
                    warn!(error = %e, "schedule_disable cascade-kill: flush failed");
                }
                info!(
                    schedule_id = %id,
                    job_id = %schedule.job_id,
                    count = exec_ids.len(),
                    "schedule disabled with cascade kill (in-flight execs signalled)",
                );
                exec_ids.len()
            }
            Err(e) => {
                warn!(error = %e, job_id = %schedule.job_id, "schedule_disable cascade-kill: in-flight exec query failed; no kills sent");
                0
            }
        }
    } else {
        0
    };

    audit::record(
        &s.nats,
        "operator",
        "schedule_disable",
        Some(&id),
        Some(&caller),
        serde_json::json!({
            "cascade": cascade_applied,
            "cascade_kill": q.cascade_kill,
            "killed_execs": killed_execs,
            "job_id": schedule.job_id,
        }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/schedules/{id}/enable
///
/// Symmetrical to [`disable`]'s soft path: flip `enabled = true` on
/// the schedule in `BUCKET_SCHEDULES` so the cron loops (backend
/// `scheduler.rs` + agent `local_scheduler.rs`) pick it back up on
/// the next watch tick. Uses the same `kv.entry().revision` +
/// `update()` optimistic-concurrency pattern as `disable` so an
/// enable click can't clobber a concurrent cron/target edit.
///
/// Note: this only re-arms the cron. It does NOT touch
/// `script_status` — a job revoked by a hard disable stays REVOKED
/// until the operator runs `kanade unrevoke <job_id>` explicitly.
/// Silently un-revoking here would defeat the point of the Layer 2
/// brake.
///
/// History: the SPA has called this endpoint since PR #38, but the
/// backend handler was lost in that PR's squash merge — every Enable
/// click 404'd. Restored here with a `schedule_enable` audit record.
pub async fn enable(
    State(s): State<AppState>,
    Path(id): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = s
        .jetstream
        .get_key_value(BUCKET_SCHEDULES)
        .await
        .map_err(|e| {
            warn!(error = %e, "schedules KV missing on enable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("schedules bucket missing: {e}"),
            )
        })?;
    let entry = kv
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

    if schedule.enabled {
        info!(schedule_id = %id, "schedule already enabled; no-op");
        return Ok(StatusCode::NO_CONTENT);
    }
    schedule.enabled = true;
    let body = serde_json::to_vec(&schedule).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize schedule: {e}"),
        )
    })?;
    kv.update(&id, body.into(), entry.revision)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV update: {e}")))?;
    sync_yaml_mirror_enabled(&s, &id, true).await;

    info!(schedule_id = %id, "schedule enabled");
    audit::record(
        &s.nats,
        "operator",
        "schedule_enable",
        Some(&id),
        Some(&caller),
        serde_json::json!({
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

#[cfg(test)]
mod tests {
    use super::patch_yaml_enabled;

    #[test]
    fn flips_existing_flag_and_preserves_comments() {
        let yaml = "# nightly inventory sweep\nid: inv-hw\ncron: \"0 3 * * *\"\nenabled: false\njob_id: inventory-hw\n";
        let out = patch_yaml_enabled(yaml, true);
        assert!(out.contains("enabled: true\n"));
        assert!(!out.contains("enabled: false"));
        // Comments and the other keys survive untouched.
        assert!(out.starts_with("# nightly inventory sweep\n"));
        assert!(out.contains("cron: \"0 3 * * *\"\n"));
    }

    #[test]
    fn drops_inline_comment_on_the_flipped_line_only() {
        let yaml = "id: s1\nenabled: true # stopped during incident\ncron: \"* * * * *\"\n";
        let out = patch_yaml_enabled(yaml, false);
        assert!(out.contains("enabled: false\n"));
        assert!(!out.contains("stopped during incident"));
        assert!(out.contains("cron: \"* * * * *\"\n"));
    }

    #[test]
    fn ignores_indented_enabled_in_nested_maps() {
        let yaml = "id: s1\ntarget:\n  enabled: false\nenabled: false\n";
        let out = patch_yaml_enabled(yaml, true);
        // Top-level flipped, nested left alone.
        assert!(out.contains("\nenabled: true\n"));
        assert!(out.contains("  enabled: false\n"));
    }

    #[test]
    fn appends_when_missing() {
        let yaml = "id: s1\ncron: \"* * * * *\"\n";
        let out = patch_yaml_enabled(yaml, false);
        assert!(out.ends_with("enabled: false\n"));
        assert!(out.starts_with("id: s1\n"));
    }

    #[test]
    fn only_first_top_level_occurrence_is_patched() {
        // Duplicate top-level keys are invalid YAML, but the patcher
        // shouldn't multiply writes if one sneaks in.
        let yaml = "enabled: false\nenabled: false\n";
        let out = patch_yaml_enabled(yaml, true);
        assert_eq!(out.matches("enabled: true").count(), 1);
    }

    // ---- schedule_run_stats (#418 coverage view) ----

    use super::schedule_run_stats;
    use chrono::{Duration, Utc};
    use sqlx::SqlitePool;

    async fn fresh_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Insert one `execution_results` row. `exit_code: None` +
    /// `finished: false` models an in-flight run. `recorded_ago_min`
    /// bounds it relative to "now". Every timestamp is bound via chrono
    /// (RFC 3339), matching how the projector writes — the `since`
    /// comparison breaks otherwise (#390).
    async fn insert_exec(
        pool: &SqlitePool,
        result_id: &str,
        job_id: &str,
        exit_code: Option<i64>,
        finished: bool,
        recorded_ago_min: i64,
    ) {
        let now = Utc::now();
        let recorded = now - Duration::minutes(recorded_ago_min);
        let started = recorded - Duration::minutes(1);
        let finished_at = finished.then_some(recorded);
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at, job_id)
             VALUES (?, 'req', 'pc-1', ?, '', '', ?, ?, ?, ?)",
        )
        .bind(result_id)
        .bind(exit_code)
        .bind(started)
        .bind(finished_at)
        .bind(recorded)
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_stats_tallies_recent_and_picks_latest() {
        let pool = fresh_pool().await;
        // job j1: an old ok (outside 24h), a fail, an ok, then a still-
        // running row that is the most recent. job j2 must not bleed in.
        insert_exec(&pool, "old", "j1", Some(0), true, 60 * 30).await; // 30h ago
        insert_exec(&pool, "fail", "j1", Some(1), true, 120).await;
        insert_exec(&pool, "ok", "j1", Some(0), true, 60).await;
        insert_exec(&pool, "running", "j1", None, false, 10).await; // latest
        insert_exec(&pool, "other", "j2", Some(1), true, 60).await;

        let since = Utc::now() - Duration::hours(24);
        let (last, recent) = schedule_run_stats(&pool, "j1", since).await.unwrap();

        // Most recent row wins, in-flight (no exit / finished) surfaced.
        let last = last.expect("j1 has runs");
        assert_eq!(last.exit_code, None);
        assert!(last.finished_at.is_none());
        // 24h tally: the ok and the fail; the 30h-old ok and the still-
        // running row are excluded; j2 is a different job.
        assert_eq!(recent.ok, 1);
        assert_eq!(recent.fail, 1);
        assert_eq!(recent.window_hours, 24);
    }

    /// Insert a finished/in-flight row for a specific pc + version, so a
    /// coverage_rows test can exercise the per-pc latest-wins SQL (the
    /// shared `insert_exec` is pinned to pc-1 / no version).
    #[allow(clippy::too_many_arguments)]
    async fn insert_run(
        pool: &SqlitePool,
        result_id: &str,
        job_id: &str,
        pc_id: &str,
        exit_code: Option<i64>,
        finished: bool,
        version: &str,
        recorded_ago_min: i64,
    ) {
        let now = Utc::now();
        let recorded = now - Duration::minutes(recorded_ago_min);
        let started = recorded - Duration::minutes(1);
        let finished_at = finished.then_some(recorded);
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, recorded_at, job_id, version)
             VALUES (?, 'req', ?, ?, '', '', ?, ?, ?, ?, ?)",
        )
        .bind(result_id)
        .bind(pc_id)
        .bind(exit_code)
        .bind(started)
        .bind(finished_at)
        .bind(recorded)
        .bind(job_id)
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn coverage_rows_picks_latest_finished_per_pc() {
        let pool = fresh_pool().await;
        // pc-a: an older ok then a newer fail (latest wins) + version.
        insert_run(&pool, "a1", "j1", "pc-a", Some(0), true, "v1", 120).await;
        insert_run(&pool, "a2", "j1", "pc-a", Some(1), true, "v2", 30).await;
        // pc-b: finished ok AND a later in-flight row → must appear in
        // BOTH the finished map (ok/v3) and the in-flight set.
        insert_run(&pool, "b1", "j1", "pc-b", Some(0), true, "v3", 60).await;
        insert_run(&pool, "b2", "j1", "pc-b", None, false, "v3", 5).await;
        // Different job must not bleed in.
        insert_run(&pool, "x1", "j2", "pc-a", Some(0), true, "v9", 10).await;

        let (inflight, finished) = coverage_rows(&pool, "j1").await.unwrap();

        assert!(inflight.contains("pc-b"));
        assert!(!inflight.contains("pc-a"));
        // pc-a latest finished is the fail at v2.
        assert_eq!(finished.get("pc-a").unwrap().0, Some(1));
        assert_eq!(finished.get("pc-a").unwrap().1.as_deref(), Some("v2"));
        // pc-b finished ok at v3 (the in-flight row is separate).
        assert_eq!(finished.get("pc-b").unwrap().0, Some(0));
        assert_eq!(finished.get("pc-b").unwrap().1.as_deref(), Some("v3"));
        // j2's pc-a row didn't leak.
        assert_eq!(finished.len(), 2);
    }

    #[tokio::test]
    async fn run_stats_empty_for_unknown_job() {
        let pool = fresh_pool().await;
        insert_exec(&pool, "x", "j1", Some(0), true, 10).await;
        let since = Utc::now() - Duration::hours(24);
        let (last, recent) = schedule_run_stats(&pool, "no-such-job", since)
            .await
            .unwrap();
        assert!(last.is_none());
        assert_eq!((recent.ok, recent.fail), (0, 0));
    }

    // ---- coverage_for (#418 rollout coverage) ----

    use super::{FinishedMap, coverage_for, coverage_rows};
    use std::collections::{HashMap, HashSet};

    fn pcs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn coverage_classifies_each_roster_pc() {
        // Roster of 5 targeted hosts. pc-ok succeeded, pc-fail failed,
        // pc-run is in-flight, pc-pend never ran, pc-off is offline and
        // never ran (still counted — that's the point of the full
        // roster). in-flight wins over a stale finished row for the
        // same pc.
        let roster = pcs(&["pc-ok", "pc-fail", "pc-run", "pc-pend", "pc-off"]);
        let inflight: HashSet<String> = ["pc-run"].iter().map(|s| s.to_string()).collect();
        let mut finished: FinishedMap = HashMap::new();
        finished.insert(
            "pc-ok".into(),
            (
                Some(0),
                Some("v1.4.3".into()),
                Some("2026-06-12T00:00:00Z".into()),
            ),
        );
        finished.insert(
            "pc-fail".into(),
            (
                Some(1),
                Some("v1.4.2".into()),
                Some("2026-06-12T00:00:00Z".into()),
            ),
        );
        // A stale finished row for the currently-running pc — running
        // must take precedence.
        finished.insert("pc-run".into(), (Some(0), None, None));

        let (agents, ok, fail, running, pending) = coverage_for(&roster, &inflight, &finished);
        assert_eq!((ok, fail, running, pending), (1, 1, 1, 2));
        assert_eq!(agents.len(), 5);

        let state = |pc: &str| agents.iter().find(|a| a.pc_id == pc).unwrap().state;
        assert_eq!(state("pc-ok"), "ok");
        assert_eq!(state("pc-fail"), "fail");
        assert_eq!(state("pc-run"), "running");
        assert_eq!(state("pc-pend"), "pending");
        assert_eq!(state("pc-off"), "pending");

        // Version is surfaced only on finished rows.
        let ver = |pc: &str| {
            agents
                .iter()
                .find(|a| a.pc_id == pc)
                .unwrap()
                .version
                .clone()
        };
        assert_eq!(ver("pc-ok").as_deref(), Some("v1.4.3"));
        assert_eq!(ver("pc-run"), None);
        assert_eq!(ver("pc-pend"), None);
    }

    #[test]
    fn coverage_all_pending_when_no_runs() {
        let roster = pcs(&["a", "b", "c"]);
        let (agents, ok, fail, running, pending) =
            coverage_for(&roster, &HashSet::new(), &HashMap::new());
        assert_eq!((ok, fail, running, pending), (0, 0, 0, 3));
        assert!(agents.iter().all(|a| a.state == "pending"));
    }

    #[test]
    fn coverage_finished_with_null_exit_is_fail() {
        // A finished row (finished_at set) but NULL exit_code is treated
        // conservatively as a failure, not a success.
        let roster = pcs(&["x"]);
        let mut finished: FinishedMap = HashMap::new();
        finished.insert(
            "x".into(),
            (None, None, Some("2026-06-12T00:00:00Z".into())),
        );
        let (_, ok, fail, _, _) = coverage_for(&roster, &HashSet::new(), &finished);
        assert_eq!((ok, fail), (0, 1));
    }

    #[test]
    fn coverage_ignores_runs_for_pcs_outside_roster() {
        // A finished run for a pc no longer in the target must not be
        // counted — the totals follow the roster, not the result table.
        let roster = pcs(&["in"]);
        let mut finished: FinishedMap = HashMap::new();
        finished.insert("in".into(), (Some(0), None, None));
        finished.insert("gone".into(), (Some(0), None, None));
        let (agents, ok, _, _, _) = coverage_for(&roster, &HashSet::new(), &finished);
        assert_eq!(ok, 1);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pc_id, "in");
    }
}
