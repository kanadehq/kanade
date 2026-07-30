//! Job endpoints:
//!   * `POST /api/jobs/{job_id}/kill` — runtime control. Looks up every
//!     in-flight execution of `{job_id}` from the `executions` table
//!     (status pending / running) and publishes `kill.{exec_id}` per
//!     deployment so agents actually receive the signal (spec §2.6
//!     Layer 3). Pre-v0.29 this published `kill.{cmd_id}`, which no
//!     agent subscribes to — the kill button on the SPA was effectively
//!     a no-op since v0.27.
//!   * `GET / POST /api/jobs` + `DELETE /api/jobs/{id}` — catalog CRUD
//!     (v0.15). Schedules reference catalog rows by `job_id`.

use std::collections::HashMap;

use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use futures::TryStreamExt;
use kanade_shared::kv::{
    BUCKET_JOBS, BUCKET_JOBS_YAML, BUCKET_SCHEDULES, BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS,
    SCRIPT_STATUS_REVOKED,
};
use kanade_shared::manifest::{ExecuteShell, InventoryHint, Manifest, RepoOrigin, Schedule};
use kanade_shared::subject;
use kanade_shared::wire::RunAs;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tracing::{info, warn};

use super::AppState;
use super::yaml_body::{YamlOrJson, mirror_yaml, yaml_headers};
use crate::audit;
use crate::audit::Caller;

pub async fn kill(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    // v0.29 / Issue #19: the agent listens on `kill.{exec_id}`, never
    // on `kill.{cmd_id}`. The path param here is the cmd / manifest
    // id, so we have to expand it to every still-running exec_id and
    // publish per-exec. status IN ('pending', 'running') skips
    // already-completed deployments — there's nothing to kill on those.
    let rows = sqlx::query(
        "SELECT exec_id FROM executions \
         WHERE job_id = ? AND status IN ('pending', 'running')",
    )
    .bind(&job_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, job_id, "kill: lookup running execs");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("lookup running execs: {e}"),
        )
    })?;

    let exec_ids: Vec<String> = rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("exec_id").unwrap_or_default())
        .filter(|s| !s.is_empty())
        .collect();

    if exec_ids.is_empty() {
        // No running deployments → there's nothing to kill. Return
        // 204 anyway: the operator's mental model is "I clicked kill,
        // it's not running", which is what 204 + zero-published
        // conveys. A 404 here would just confuse the SPA.
        info!(
            %job_id,
            "kill: no running executions for this job (no-op)",
        );
        return Ok(StatusCode::NO_CONTENT);
    }

    for exec_id in &exec_ids {
        if let Err(e) = state
            .nats
            .publish(subject::kill(exec_id), bytes::Bytes::new())
            .await
        {
            warn!(error = %e, %job_id, %exec_id, "publish kill failed");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("publish kill.{exec_id}: {e}"),
            ));
        }
    }
    // flush so the subjects are on the wire before we ack the
    // operator — without it, a fast operator-then-shutdown could
    // theoretically drop the kill on the floor.
    let _ = state.nats.flush().await;
    info!(
        %job_id,
        kill_count = exec_ids.len(),
        "kill signal fanned out to running execs",
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct JobSummary {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
    pub inventory: bool,
}

/// v0.30 / PR γ: in-flight counters joined onto each `/api/jobs`
/// row so the Jobs page can show "is anything running right now"
/// at a glance — the operator's decision input for kill / revoke
/// without having to drill into Activity. Sourced from
/// `executions.status`, which the v0.29 results projector now
/// maintains correctly.
#[derive(Serialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct JobLiveCounts {
    /// `executions.status = 'running'` — at least one result has
    /// landed but more are still in flight.
    pub running: i64,
    /// `executions.status = 'pending'` — fan-out published but no
    /// result has landed yet. Distinguished from `running` so the
    /// operator can tell "nothing reported back" from "partially
    /// reported".
    pub pending: i64,
}

/// The `execute:` fields the Jobs page's catalog table renders
/// (`web/src/pages/Jobs.tsx` `JobRow`). The full `Execute` is NOT
/// serialised here — `execute.script` bodies were ~85 % of the list
/// payload (#1214①) and neither list consumer (Jobs page, Exec
/// picker) reads them. The YAML editor fetches the full source via
/// `GET /api/jobs/{id}/yaml` instead.
#[derive(Serialize)]
pub struct ExecuteSummary {
    pub shell: ExecuteShell,
    pub timeout: String,
    pub run_as: RunAs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `GET /api/jobs` row shape — a projection of the registered
/// Manifest carrying only the fields the SPA's two list consumers
/// read, plus the `live` counters aggregated from `executions`.
/// Field-for-field this keeps the old flattened shape (`job.id`,
/// `job.execute.shell`, …), so the SPA needs no change; what changed
/// is what's absent: `execute.script` / `script_file` /
/// `script_object` and the hint blocks the table never renders
/// (#1214①, ~500 KB → ~75 KB on the measured catalog).
///
/// `kanade job list` pretty-prints this verbatim — dropping the
/// script body from its output is the deliberate compatibility call
/// from #1214: `list` is a catalog overview and
/// `GET /api/jobs/{id}/yaml` remains the path to a full manifest.
#[derive(Serialize)]
pub struct JobListRow {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
    pub execute: ExecuteSummary,
    pub inventory: Option<InventoryHint>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<RepoOrigin>,
    pub live: JobLiveCounts,
}

impl JobListRow {
    fn from_manifest(m: Manifest, live: JobLiveCounts) -> Self {
        Self {
            id: m.id,
            version: m.version,
            description: m.description,
            execute: ExecuteSummary {
                shell: m.execute.shell,
                timeout: m.execute.timeout,
                run_as: m.execute.run_as,
                cwd: m.execute.cwd,
            },
            inventory: m.inventory,
            tags: m.tags,
            origin: m.origin,
            live,
        }
    }
}

/// GET /api/jobs — list every registered job + live in-flight
/// counters from the executions table.
pub async fn list(
    State(s): State<AppState>,
) -> Result<Json<Vec<JobListRow>>, (StatusCode, String)> {
    let kv = match s.jetstream.get_key_value(BUCKET_JOBS).await {
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
    // #1214②: fan the per-key GETs out in parallel — the previous
    // sequential loop serialised one NATS round-trip per catalog row
    // on the Jobs page's critical path. Mirrors scripts::list_status
    // ("Gemini #47 review"), bounded by the operator-facing catalog
    // size (~10s-100s of jobs).
    let fetches = keys.into_iter().map(|k| {
        let kv = kv.clone();
        async move {
            match kv.get(&k).await {
                Ok(Some(bytes)) => serde_json::from_slice::<Manifest>(&bytes).ok(),
                _ => None,
            }
        }
    });
    let mut manifests: Vec<Manifest> = futures::future::join_all(fetches)
        .await
        .into_iter()
        .flatten()
        .collect();
    manifests.sort_by(|a, b| a.id.cmp(&b.id));

    // v0.30 / PR γ: one GROUP BY query for the whole list instead of
    // N round-trips. `executions` lives in SQLite so this is local.
    // A missing `executions` row for a job (= never fired since the
    // backend started) yields the default zeros via the HashMap
    // lookup fallback below.
    let live_counts = fetch_live_counts(&s.pool).await.unwrap_or_else(|e| {
        warn!(error = %e, "jobs list: live count aggregation failed; returning zeros");
        HashMap::new()
    });

    let out: Vec<JobListRow> = manifests
        .into_iter()
        .map(|m| {
            let live = live_counts.get(&m.id).cloned().unwrap_or_default();
            JobListRow::from_manifest(m, live)
        })
        .collect();
    Ok(Json(out))
}

/// Aggregate `executions.status` counts by `job_id` so the
/// `/api/jobs` list can attach per-row live counters in one round
/// trip. Returned map omits jobs with no executions entirely; the
/// caller falls back to `JobLiveCounts::default()`.
async fn fetch_live_counts(
    pool: &SqlitePool,
) -> Result<HashMap<String, JobLiveCounts>, sqlx::Error> {
    // Gemini #71 perf fix: filter on status BEFORE the aggregation
    // so SQLite skips completed rows entirely instead of summing
    // CASE-zeros for the (growing forever) historical tail. The
    // resulting empty groups disappear from the output map; the
    // caller already falls back to `JobLiveCounts::default()` for
    // jobs not present in the map, so semantics are preserved.
    let rows = sqlx::query(
        "SELECT job_id,
                SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END) AS running,
                SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END) AS pending
           FROM executions
          WHERE status IN ('running', 'pending')
          GROUP BY job_id",
    )
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<String, JobLiveCounts> = HashMap::with_capacity(rows.len());
    for r in rows {
        let job_id: String = r.try_get("job_id").unwrap_or_default();
        if job_id.is_empty() {
            continue;
        }
        out.insert(
            job_id,
            JobLiveCounts {
                running: r.try_get("running").unwrap_or(0),
                pending: r.try_get("pending").unwrap_or(0),
            },
        );
    }
    Ok(out)
}

/// POST /api/jobs — upsert a Manifest into the job catalog. The KV
/// key is `manifest.id`.
///
/// Accepts JSON (`application/json`, default) or YAML
/// (`application/yaml`, `text/yaml`). When the body is YAML, the raw
/// source is mirrored verbatim into `BUCKET_JOBS_YAML` so the SPA's
/// YAML editor preserves operator comments + script block-scalar
/// indentation across edits. JSON callers get a `serde_yaml::to_string`
/// fallback so the YAML bucket stays in lockstep — the operator just
/// loses comment fidelity on that path (it has nothing to preserve).
pub async fn create(
    State(s): State<AppState>,
    caller: Caller,
    body: YamlOrJson<Manifest>,
) -> Result<Json<JobSummary>, (StatusCode, String)> {
    let YamlOrJson {
        value: job,
        raw_yaml,
    } = body;
    // SPEC §2.4.1: exactly one of script / script_file /
    // script_object must be set. Enforce at the write boundary
    // so the JOBS KV never stores ambiguous manifests.
    job.validate().map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // #918: `script_file` is CLI-side sugar — `kanade job create`
    // reads the repo-local file and inlines it into `script` before
    // POSTing, so a manifest that reaches this handler still carrying
    // `script_file` came in through the raw API / SPA editor, where no
    // filesystem resolution ever happens. Storing it would 400 at
    // every exec (`resolve_script_source`), so reject it here with an
    // actionable message rather than accepting a job that can never
    // run.
    if job.execute.script_file.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "execute.script_file is resolved CLI-side by `kanade job create` and cannot be \
             stored via the API — inline the script into `execute.script`, or publish it as a \
             `script_object`"
                .to_string(),
        ));
    }
    let kv = s
        .jetstream
        .create_key_value(KvConfig {
            bucket: BUCKET_JOBS.into(),
            history: 5,
            ..Default::default()
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ensure KV: {e}")))?;
    let body_bytes = serde_json::to_vec(&job)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
    kv.put(&job.id, body_bytes.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;

    // Keep `script_current` in lockstep with the catalog on every upsert,
    // NOT just on `kanade exec` (exec.rs #258). `script_current.<id>` is
    // the canonical "current version" the agent's Layer 2 version-pin gate
    // (§2.6.2) checks against. Previously only the exec / backend-fire path
    // wrote it, so a `runs_on: agent` job — which fires from its own
    // BUCKET_JOBS cache and never routes through exec_manifest — left the
    // KV frozen at whatever version was last exec'd. After a version bump
    // the KV lagged the catalog until someone ran a manual exec, and the
    // agent's own fresh fires got self-rejected (exit 124, version-pin
    // mismatch). Writing it here means the pin baseline tracks the catalog
    // regardless of `runs_on`. Best-effort: a failure must not roll back
    // the catalog write above (the agent-local path no longer depends on
    // this KV — see CommandSource::LocalScheduler — and the backend-fire
    // path re-pins it in exec_manifest anyway), so warn and continue.
    match s.jetstream.get_key_value(BUCKET_SCRIPT_CURRENT).await {
        Ok(cur) => {
            if let Err(e) = cur
                .put(
                    &job.id,
                    bytes::Bytes::from(job.version.clone().into_bytes()),
                )
                .await
            {
                warn!(error = %e, job_id = %job.id, "jobs: script_current put failed; catalog is current");
            }
        }
        Err(e) => {
            warn!(error = %e, "jobs: script_current KV missing; skipping version-pin refresh")
        }
    }

    // Operator-facing YAML mirror — best-effort. A failure here doesn't
    // roll back the JSON catalog (which the scheduler / agents read)
    // because the user's update is functionally already in place; we
    // just may lose a round of comment preservation. Warn-log so the
    // gap is observable.
    let yaml_source = raw_yaml.unwrap_or_else(|| {
        serde_yaml::to_string(&job)
            .unwrap_or_else(|_| String::from("# YAML mirror unavailable for this entry"))
    });
    if let Err(e) = mirror_yaml(&s, BUCKET_JOBS_YAML, &job.id, &yaml_source).await {
        warn!(error = %e, job_id = %job.id, "jobs: YAML mirror put failed; JSON catalog is current");
    }

    let summary = JobSummary {
        id: job.id.clone(),
        version: job.version.clone(),
        description: job.description.clone(),
        inventory: job.inventory.is_some(),
    };
    info!(job_id = %job.id, version = %job.version, "job upserted");
    audit::record(
        &s.nats,
        "operator",
        "job_upsert",
        Some(&job.id),
        Some(&caller),
        serde_json::json!({
            "version": job.version,
            "inventory": job.inventory.is_some(),
        }),
    )
    .await;
    Ok(Json(summary))
}

/// `GET /api/jobs/{id}/yaml` — fetch the operator's YAML source for
/// the job, used by the SPA editor to populate Edit modals without
/// losing comments / block-scalar formatting. Falls back to a
/// `serde_yaml::to_string` dump of the JSON catalog row when the
/// YAML mirror is missing (legacy entries from before this endpoint).
pub async fn get_yaml(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, HeaderMap, String), (StatusCode, String)> {
    if let Ok(kv) = s.jetstream.get_key_value(BUCKET_JOBS_YAML).await
        && let Ok(Some(bytes)) = kv.get(&id).await
        && let Ok(yaml_str) = String::from_utf8(bytes.to_vec())
    {
        return Ok((StatusCode::OK, yaml_headers(), yaml_str));
    }

    let kv = s
        .jetstream
        .get_key_value(BUCKET_JOBS)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, format!("job '{id}' not found")))?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("job '{id}' not found")))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("decode: {e}")))?;
    let yaml = serde_yaml::to_string(&manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode YAML: {e}"),
        )
    })?;
    Ok((StatusCode::OK, yaml_headers(), yaml))
}

/// DELETE /api/jobs/{id} — 409 if any Schedule references it.
///
/// v0.27 (SPEC §2.6.4 (b)) cascades a Layer 2 revoke: before the
/// Manifest is removed from `BUCKET_JOBS`, the handler writes
/// `script_status.{id} = REVOKED` so any Command already in flight
/// (live core sub delivery in progress, or stored in `STREAM_EXEC`
/// awaiting a reconnecting agent) gets skipped by the agent's
/// `handle_command` KV check. Without this, deleting a Manifest only
/// stops *future* exec calls — already-published Commands would
/// still run.
///
/// To undo the cascade: re-create the Manifest with
/// `kanade job create`, then `kanade unrevoke <id>` to flip
/// `script_status` back to `ACTIVE`.
const CASCADE_FACTS_SQL: &str = "DELETE FROM inventory_facts WHERE job_id = ?";
const CASCADE_HISTORY_SQL: &str = "DELETE FROM inventory_history WHERE job_id = ?";

/// Drop a job's inventory rows (facts + history), keyed by `job_id`.
/// Both deletes run in one transaction so the two tables can't end up
/// half-cleared if the second fails. Returns `(facts_deleted,
/// history_deleted)`. The `delete` handler runs this AND the
/// `delete_cascades_inventory_for_that_job_only` test calls it directly,
/// so the test exercises the real SQL (a table/column typo fails the
/// test rather than silently passing reimplemented SQL).
async fn cascade_inventory(pool: &SqlitePool, job_id: &str) -> sqlx::Result<(u64, u64)> {
    let mut tx = pool.begin().await?;
    let facts = sqlx::query(CASCADE_FACTS_SQL)
        .bind(job_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let history = sqlx::query(CASCADE_HISTORY_SQL)
        .bind(job_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok((facts, history))
}

pub async fn delete(
    State(s): State<AppState>,
    Path(id): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    if let Ok(kv) = s.jetstream.get_key_value(BUCKET_SCHEDULES).await
        && let Ok(keys_stream) = kv.keys().await
    {
        let keys: Vec<String> = keys_stream.try_collect().await.unwrap_or_default();
        for k in keys {
            if let Ok(Some(bytes)) = kv.get(&k).await
                && let Ok(sched) = serde_json::from_slice::<Schedule>(&bytes)
                && sched.job_id == id
            {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "job '{id}' is referenced by schedule '{}'; remove the schedule first",
                        sched.id
                    ),
                ));
            }
        }
    }

    // v0.27 — SPEC §2.6.4 (b) cascade revoke: every job delete also
    // writes `script_status.{cmd_id} = REVOKED` so any in-flight
    // Command for this manifest (publish-already-emitted but the
    // agent hasn't run yet, or about to be replayed from STREAM_EXEC
    // on reconnect) gets caught by the Layer 2 KV check and skipped.
    // Without this, removing a Manifest only stops *future* exec
    // calls — Commands already in the broker would still execute on
    // any agent that reads them. We revoke FIRST, then delete the
    // Manifest, so that if delete somehow fails we're still in a safe
    // (revoked) state. Idempotent — re-revoking an already-REVOKED
    // entry is a no-op put. v0.27 round-2 review (gemini #36
    // line 208): resolve BOTH KV handles upfront before any write —
    // that way a missing / unreachable BUCKET_JOBS surfaces as a
    // clean 404 with zero side effects, instead of leaking a revoke
    // that has no matching delete.
    let status_kv = s
        .jetstream
        .get_key_value(BUCKET_SCRIPT_STATUS)
        .await
        .map_err(|e| {
            warn!(
                error = %e,
                bucket = BUCKET_SCRIPT_STATUS,
                "job_delete cascade revoke: status KV unavailable",
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("script_status bucket missing: {e}"),
            )
        })?;
    let kv = s.jetstream.get_key_value(BUCKET_JOBS).await.map_err(|e| {
        warn!(error = %e, "jobs KV missing on delete");
        (StatusCode::NOT_FOUND, "jobs bucket missing".to_string())
    })?;

    status_kv
        .put(&id, bytes::Bytes::from(SCRIPT_STATUS_REVOKED))
        .await
        .map_err(|e| {
            warn!(
                error = %e,
                job_id = %id,
                bucket = BUCKET_SCRIPT_STATUS,
                "job_delete cascade revoke: status KV put failed",
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("script_status put: {e}"),
            )
        })?;
    // If the manifest delete fails *after* we successfully cascaded
    // the revoke, the operator needs to know that `script_status.{id}`
    // is now REVOKED so they can `kanade unrevoke <id>` as part of the
    // recovery. We audit + surface the revoke state in the error body
    // rather than silently dropping it (CodeRabbit #36 review).
    if let Err(e) = kv.delete(&id).await {
        warn!(
            error = %e,
            job_id = %id,
            cascade_revoke = true,
            "job_delete failed after cascade revoke",
        );
        audit::record(
            &s.nats,
            "operator",
            "job_delete_failed_post_revoke",
            Some(&id),
            Some(&caller),
            serde_json::json!({ "cascade_revoke": true, "error": e.to_string() }),
        )
        .await;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "kv delete: {e}; script_status.{id} is already REVOKED — `kanade unrevoke {id}` to recover"
            ),
        ));
    }
    // Cascade inventory cleanup. inventory_facts / inventory_history are
    // keyed by job_id, so this job's rows are unambiguously its own —
    // unlike check_status (keyed by check slug, deliberately NOT cascaded
    // so a same-named replacement job keeps its rows). Without this a
    // deleted inventory job leaves orphaned facts stuck under a dead
    // job_id on the SPA — there is no other cleanup path for
    // inventory_facts (inventory_history additionally has a 90d TTL).
    //
    // Best-effort: the manifest is already gone from KV, so a failed
    // sweep must not fail the request — that would only mean orphans
    // remain (the pre-cascade status quo). Surface it via warn + audit
    // rather than a misleading 500 for an already-deleted job.
    let (facts_deleted, history_deleted, cascade_error) =
        match cascade_inventory(&s.pool, &id).await {
            Ok((f, h)) => (f, h, None),
            Err(e) => {
                warn!(error = %e, job_id = %id, "job_delete inventory cascade failed");
                (0, 0, Some(e.to_string()))
            }
        };

    info!(
        job_id = %id,
        cascade_revoke = true,
        inventory_facts_deleted = facts_deleted,
        inventory_history_deleted = history_deleted,
        cascade_ok = cascade_error.is_none(),
        "job deleted",
    );
    audit::record(
        &s.nats,
        "operator",
        "job_delete",
        Some(&id),
        Some(&caller),
        serde_json::json!({
            "cascade_revoke": true,
            "inventory_facts_deleted": facts_deleted,
            "inventory_history_deleted": history_deleted,
            // null on success; an error string distinguishes a failed
            // sweep (orphans may remain) from a genuine "0 rows matched".
            "inventory_cascade_error": cascade_error,
        }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Lookup helper for scheduler + projector. Returns `Ok(None)` when
/// the key is absent so callers can warn-and-skip without unwrapping
/// a fatal error.
pub async fn fetch(
    js: &async_nats::jetstream::Context,
    job_id: &str,
) -> anyhow::Result<Option<Manifest>> {
    let kv = match js.get_key_value(BUCKET_JOBS).await {
        Ok(k) => k,
        Err(_) => return Ok(None),
    };
    let Some(bytes) = kv.get(job_id).await? else {
        return Ok(None);
    };
    let job: Manifest = serde_json::from_slice(&bytes)?;
    Ok(Some(job))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open sqlite memory");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn insert_exec(
        pool: &SqlitePool,
        exec_id: &str,
        job_id: &str,
        status: &str,
        target: i64,
    ) {
        sqlx::query(
            "INSERT INTO executions
                (exec_id, job_id, version, initiated_by, target_count, status)
             VALUES (?, ?, '1.0.0', 'tester', ?, ?)",
        )
        .bind(exec_id)
        .bind(job_id)
        .bind(target)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fetch_live_counts_groups_by_job_id() {
        // Three execs for "inv-hw" (2 running, 1 pending), one exec for
        // "patch-x" (just running). The aggregation should produce a
        // map keyed by job_id with the right partition.
        let pool = fresh_pool().await;
        insert_exec(&pool, "e1", "inv-hw", "running", 10).await;
        insert_exec(&pool, "e2", "inv-hw", "running", 10).await;
        insert_exec(&pool, "e3", "inv-hw", "pending", 10).await;
        insert_exec(&pool, "e4", "patch-x", "running", 5).await;

        let counts = fetch_live_counts(&pool).await.unwrap();
        assert_eq!(
            counts.get("inv-hw"),
            Some(&JobLiveCounts {
                running: 2,
                pending: 1,
            }),
        );
        assert_eq!(
            counts.get("patch-x"),
            Some(&JobLiveCounts {
                running: 1,
                pending: 0,
            }),
        );
    }

    #[tokio::test]
    async fn fetch_live_counts_excludes_completed() {
        // 'completed' status (= projector saw all target_count
        // results) shouldn't count toward live. Only 'running' and
        // 'pending' are operationally "in flight".
        let pool = fresh_pool().await;
        insert_exec(&pool, "e1", "j", "completed", 5).await;
        insert_exec(&pool, "e2", "j", "running", 5).await;

        let counts = fetch_live_counts(&pool).await.unwrap();
        let live = counts.get("j").expect("j has at least one exec");
        assert_eq!(live.running, 1);
        assert_eq!(live.pending, 0);
    }

    #[tokio::test]
    async fn fetch_live_counts_empty_when_no_executions() {
        let pool = fresh_pool().await;
        let counts = fetch_live_counts(&pool).await.unwrap();
        assert!(counts.is_empty());
    }

    async fn insert_inv_fact(pool: &SqlitePool, pc: &str, job: &str) {
        sqlx::query("INSERT INTO inventory_facts (pc_id, job_id, facts_json) VALUES (?, ?, '{}')")
            .bind(pc)
            .bind(job)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn insert_inv_hist(pool: &SqlitePool, pc: &str, job: &str) {
        sqlx::query(
            "INSERT INTO inventory_history (pc_id, job_id, field_path, change_kind)
             VALUES (?, ?, 'x', 'added')",
        )
        .bind(pc)
        .bind(job)
        .execute(pool)
        .await
        .unwrap();
    }

    // Calls the real `cascade_inventory` the handler uses (not
    // reimplemented SQL), so a table/column typo fails here: dropping a
    // job's inventory must hit only that job_id, leaving other jobs' rows.
    #[tokio::test]
    async fn delete_cascades_inventory_for_that_job_only() {
        let pool = fresh_pool().await;
        insert_inv_fact(&pool, "pc-1", "inv-hw").await;
        insert_inv_fact(&pool, "pc-2", "inv-hw").await;
        insert_inv_fact(&pool, "pc-1", "inv-sw").await; // a different job
        insert_inv_hist(&pool, "pc-1", "inv-hw").await;
        insert_inv_hist(&pool, "pc-1", "inv-sw").await;

        let (facts, hist) = cascade_inventory(&pool, "inv-hw").await.unwrap();
        assert_eq!(facts, 2, "both inv-hw fact rows cleared");
        assert_eq!(hist, 1, "inv-hw history row cleared");

        // inv-sw is untouched.
        let facts_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inventory_facts")
            .fetch_one(&pool)
            .await
            .unwrap();
        let hist_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inventory_history")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(facts_left, 1, "the other job's fact survives");
        assert_eq!(hist_left, 1, "the other job's history survives");
    }

    /// #1214①: the list response must NOT carry the script body (it
    /// was ~85 % of the payload) while keeping the flattened shape
    /// the SPA's `JobRow` reads.
    #[test]
    fn job_list_row_omits_script_but_keeps_catalog_shape() {
        let m: Manifest = serde_yaml::from_str(
            "id: inv-hw\n\
             version: 1.2.3\n\
             description: hardware inventory\n\
             execute:\n\
             \x20 shell: powershell\n\
             \x20 timeout: 30s\n\
             \x20 run_as: system\n\
             \x20 cwd: C:\\Temp\n\
             \x20 script: |\n\
             \x20   Get-ComputerInfo | Out-Null\n\
             tags: [inventory, windows]\n",
        )
        .unwrap();
        let row = JobListRow::from_manifest(
            m,
            JobLiveCounts {
                running: 2,
                pending: 1,
            },
        );
        let v = serde_json::to_value(&row).unwrap();

        assert_eq!(v["id"], "inv-hw");
        assert_eq!(v["version"], "1.2.3");
        assert_eq!(v["description"], "hardware inventory");
        assert_eq!(v["execute"]["shell"], "powershell");
        assert_eq!(v["execute"]["timeout"], "30s");
        assert_eq!(v["execute"]["run_as"], "system");
        assert_eq!(v["execute"]["cwd"], "C:\\Temp");
        assert_eq!(v["tags"], serde_json::json!(["inventory", "windows"]));
        assert_eq!(v["live"], serde_json::json!({"running": 2, "pending": 1}));
        // Serialized as null (not omitted) — the SPA types it as
        // `unknown | null`.
        assert!(v.get("inventory").is_some());
        assert!(v["inventory"].is_null());

        // The whole point of #1214①: no script source in the list row.
        assert!(v["execute"].get("script").is_none());
        assert!(v["execute"].get("script_file").is_none());
        assert!(v["execute"].get("script_object").is_none());
    }
}
