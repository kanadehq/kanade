use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::ExecResult;
use kanade_shared::kv::{BUCKET_JOBS, OBJECT_RESULT_OUTPUT, STREAM_RESULTS};
use kanade_shared::manifest::{CheckHint, InventoryHint, Manifest};
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

// pub(crate): consumer_reset::reset_if_wiped names this durable when
// deciding what to drop after a projection-DB wipe (#389).
pub(crate) const CONSUMER_NAME: &str = "backend_results_projector";

/// Consume the RESULTS stream and:
///   1. Insert each `ExecResult` into `execution_results`. The PK is
///      now `result_id` (v0.29 / Issue #19) — agent-minted per
///      (Command, PC) — so broadcast Commands with N PC replies finally
///      persist all N rows instead of dropping all but the first.
///      Redeliveries from JetStream still dedupe via
///      `ON CONFLICT(result_id) DO NOTHING`.
///   2. v0.29 / Issue #19: when the result carries an `exec_id`, bump
///      the matching `executions` row's `success_count` /
///      `failure_count` and recompute its `status` (pending → running
///      while results trickle in, completed once we've seen
///      `target_count` replies). This wires up counters that have
///      sat at 0 since v0.16.
///   3. v0.15: if the result carries a `manifest_id` AND a job
///      with that id exists in the catalog AND the job carries an
///      `inventory:` hint AND `exit_code == 0`, parse stdout as JSON
///      and upsert into `inventory_facts`.
pub async fn run(js: jetstream::Context, pool: SqlitePool) -> Result<()> {
    let stream = js
        .get_stream(STREAM_RESULTS)
        .await
        .with_context(|| format!("get stream {STREAM_RESULTS}"))?;
    let consumer = stream
        .get_or_create_consumer(
            CONSUMER_NAME,
            PullConfig {
                durable_name: Some(CONSUMER_NAME.into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .context("create results consumer")?;
    info!(
        stream = STREAM_RESULTS,
        consumer = CONSUMER_NAME,
        "results projector started"
    );

    // KV handle for job-catalog lookups. Cached here so the per-result
    // hot path doesn't repeatedly call get_key_value (which round-
    // trips to the broker).
    let jobs_kv = js
        .get_key_value(BUCKET_JOBS)
        .await
        .with_context(|| format!("get KV {BUCKET_JOBS}"))?;

    let mut messages = consumer
        .messages()
        .await
        .context("subscribe results messages")?;
    while let Some(msg) = messages.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "results consumer error");
                continue;
            }
        };
        // #398: recorded_at = the message's JetStream publish time,
        // so a -WipeDb re-projection (#389) reproduces the original
        // arrival times instead of stamping everything "now".
        let recorded_at = super::publish_time(&msg);
        match serde_json::from_slice::<ExecResult>(&msg.payload) {
            Ok(mut r) => {
                // #227: deref overflow pointers BEFORE projection.
                // Agent uploads stdout / stderr > 256 KB into
                // OBJECT_RESULT_OUTPUT and clears the inline field;
                // here we fetch the bytes back so SQLite + the SPA
                // Activity page see the full text. Pointer-less
                // results (the common small-output case + every
                // pre-#227 payload) skip the bucket fetch entirely.
                if let Err(e) = deref_overflow(&js, &mut r).await {
                    warn!(
                        error = %e,
                        request_id = %r.request_id,
                        "results: failed to deref OBJECT_RESULT_OUTPUT pointer — \
                         row will land with empty stdout/stderr (#227)",
                    );
                    // Continue with the (empty) inline fields rather
                    // than NACK — repeated NACK + redelivery would
                    // pin the consumer behind one broken row. The
                    // operator can re-fetch via the Object Store
                    // directly if the row needs to be re-projected.
                }

                // Resolve once and reuse for log + insert. For v0.29+
                // agents this is just `r.result_id`; for legacy
                // payloads this is the deterministic UUIDv5 derived
                // from (request_id, pc_id).
                let resolved_id = r.stable_result_id();
                match insert_result(&pool, &r, &resolved_id, recorded_at).await {
                    Ok(true) => {
                        info!(
                            result_id = %resolved_id,
                            request_id = %r.request_id,
                            exec_id = ?r.exec_id,
                            pc_id = %r.pc_id,
                            exit_code = r.exit_code,
                            "projected result",
                        );
                        // Only bump exec counters on a fresh insert.
                        // Redeliveries (`Ok(false)` below) must not
                        // double-count — JetStream redelivers on ack
                        // timeout, and `executions.success_count` is
                        // an unconditional `+= 1`.
                        if let Some(exec_id) = r.exec_id.as_deref() {
                            if let Err(e) = bump_exec_counters(&pool, exec_id, r.exit_code).await {
                                warn!(
                                    error = %e,
                                    exec_id,
                                    "executions counter update failed",
                                );
                            }
                        }
                    }
                    Ok(false) => {
                        info!(
                            result_id = %resolved_id,
                            "duplicate result (ON CONFLICT) — skipping counter bump",
                        );
                    }
                    Err(e) => {
                        warn!(error = %e, result_id = %resolved_id, "insert result failed");
                    }
                }
                if r.exit_code == 0 {
                    if let Err(e) = maybe_project_inventory(&pool, &jobs_kv, &r, recorded_at).await
                    {
                        warn!(error = ?e, result_id = %resolved_id, "inventory fact projection failed");
                    }
                }
                // #290 PR-E: a `check:` job (fleet != false) projects a
                // fleet-wide compliance row on EVERY exit — a clean run
                // carries the reported status, a crash projects
                // `unknown` (mirrors the agent's Health-tab behaviour),
                // so the SPA never shows a stale green for a check that
                // has started failing.
                if let Err(e) = maybe_project_check_status(&pool, &jobs_kv, &r, recorded_at).await {
                    warn!(error = ?e, result_id = %resolved_id, "check status projection failed");
                }
            }
            Err(e) => warn!(error = %e, subject = %msg.subject, "deserialize ExecResult"),
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack results message");
        }
    }
    Ok(())
}

/// v0.30 / PR α' UPSERT one ExecResult. Returns `Ok(true)` when the
/// row's `finished_at` was set by this call (= either a fresh
/// INSERT, OR an UPDATE that transitioned an in-flight row to
/// finished). Returns `Ok(false)` when the row already had
/// `finished_at` set (= JetStream redelivery of a result we already
/// projected); the caller uses this to gate the `executions`
/// counter bump so redeliveries don't double-count.
///
/// Three states this handles:
///   1. **No row yet** (race: ExecResult lands before its matching
///      events.started): INSERT creates a finished row directly.
///      events.started's later redelivery ON CONFLICT-no-ops on
///      result_id, leaving the finished row untouched.
///   2. **In-flight row exists** (normal: events.started got there
///      first): UPDATE flips finished_at from NULL to set, plus
///      copies the exit_code / stdout / stderr / manifest_id. The
///      `WHERE finished_at IS NULL` guard on the DO UPDATE prevents
///      redelivery of the same result from re-running this branch.
///   3. **Already-finished row** (redelivery): ON CONFLICT DO
///      UPDATE clause's WHERE doesn't match → rows_affected = 0 →
///      caller skips counter bump.
///   4. **Reaped placeholder** (#332): the cleanup task stamped
///      `finished_at` + `reaped = 1` on an in-flight row whose result
///      never arrived. If the *real* result then shows up late (outage
///      / partition heal / a job that ran past the 24 h reap window),
///      the `OR reaped = 1` disjunct lets it overwrite the placeholder
///      and the SET clears `reaped` back to 0 — so the real output
///      replaces the "[backend: reaped …]" note instead of being
///      silently dropped (gemini review, PR #332). A subsequent
///      redelivery then hits state 3 (finished, reaped = 0) and no-ops.
async fn insert_result(
    pool: &SqlitePool,
    r: &ExecResult,
    result_id: &str,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    // `result_id` is pre-resolved by the caller via
    // `r.stable_result_id()`: agent-supplied for v0.29+ payloads,
    // deterministic UUIDv5 from (request_id, pc_id) for legacy.
    // Determinism is load-bearing: JetStream redelivery of the same
    // legacy payload must hash to the same id so the ON CONFLICT
    // path triggers the WHERE-guard rather than inserting a second
    // row.
    // #390: `recorded_at` is bound explicitly (RFC 3339 via chrono)
    // instead of falling back to the column's DEFAULT
    // CURRENT_TIMESTAMP — the DEFAULT's space-separated text breaks
    // lexicographic `recorded_at >= ?` filters against chrono binds.
    // #398: the value is the message's JetStream publish time (see
    // `projector::publish_time`), re-projection-stable by design.
    // The conflict path intentionally leaves recorded_at at its
    // first-insert value, same as before.
    let rows = sqlx::query(
        "INSERT INTO execution_results (
             result_id, request_id, exec_id, pc_id, exit_code,
             stdout, stderr, started_at, finished_at, job_id,
             recorded_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(result_id) DO UPDATE SET
             exit_code   = excluded.exit_code,
             stdout      = excluded.stdout,
             stderr      = excluded.stderr,
             finished_at = excluded.finished_at,
             reaped      = 0,
             job_id      = COALESCE(excluded.job_id, execution_results.job_id),
             request_id  = excluded.request_id
          WHERE execution_results.finished_at IS NULL
             OR execution_results.reaped = 1",
    )
    .bind(result_id)
    .bind(&r.request_id)
    .bind(&r.exec_id)
    .bind(&r.pc_id)
    .bind(r.exit_code as i64)
    .bind(&r.stdout)
    .bind(&r.stderr)
    .bind(r.started_at)
    .bind(r.finished_at)
    .bind(&r.manifest_id)
    .bind(recorded_at)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() > 0)
}

/// v0.29 / Issue #19: update the `executions` row this result belongs
/// to. exit_code 0 → success_count++, anything else → failure_count++.
/// Status promotes pending → running on the first result, and tips to
/// `completed` once success+failure >= target_count. Uses a single
/// UPDATE with conditional CASE expressions so the row's
/// success/failure/status all change atomically without a follow-up
/// query — important because the projector is concurrent with
/// redeliveries.
async fn bump_exec_counters(pool: &SqlitePool, exec_id: &str, exit_code: i32) -> Result<()> {
    let is_success = if exit_code == 0 { 1i64 } else { 0i64 };
    let is_failure = 1 - is_success;
    sqlx::query(
        "UPDATE executions
            SET success_count = success_count + ?,
                failure_count = failure_count + ?,
                status = CASE
                    WHEN (success_count + ?) + (failure_count + ?) >= target_count
                        THEN 'completed'
                    ELSE 'running'
                END
          WHERE exec_id = ?",
    )
    .bind(is_success)
    .bind(is_failure)
    .bind(is_success)
    .bind(is_failure)
    .bind(exec_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// #227: when the agent overflowed stdout / stderr into
/// `OBJECT_RESULT_OUTPUT`, fetch the bytes back and put them into
/// the inline fields the rest of the projector + SQLite expect.
/// Pointer-less results (the common small-output case + every
/// pre-#227 payload) short-circuit so the bucket isn't touched at
/// all on the hot path.
///
/// One bucket lookup per overflowed field. Each Object Store get
/// is bounded — async-nats streams chunks back over a transient
/// consumer that completes when the metadata-recorded `chunks`
/// count is exhausted. No timeout wrapper here yet; if a wedged
/// bucket ever pins the projector we'd add one (matching the
/// `ACK_TIMEOUT` shape in the agent's outbox.rs).
async fn deref_overflow(js: &async_nats::jetstream::Context, r: &mut ExecResult) -> Result<()> {
    if r.stdout_object.is_none() && r.stderr_object.is_none() {
        return Ok(());
    }
    let store = js
        .get_object_store(OBJECT_RESULT_OUTPUT)
        .await
        .with_context(|| format!("get_object_store {OBJECT_RESULT_OUTPUT}"))?;

    // `Option::take` moves the String out + leaves None in one shot,
    // saving the explicit follow-up `r.stdout_object = None` plus a
    // redundant `clone` of the key just to satisfy the borrow checker
    // (Gemini #282 MEDIUM).
    if let Some(key) = r.stdout_object.take() {
        let bytes = read_object(&store, &key)
            .await
            .with_context(|| format!("deref stdout_object {key}"))?;
        // The bucket holds UTF-8 (it came from PowerShell's UTF-8
        // capture in process.rs). `from_utf8_lossy` keeps a single
        // malformed byte from killing the whole row — same posture
        // as the agent's capture side, which has used `from_utf8_lossy`
        // since the CP932 incident.
        r.stdout = String::from_utf8_lossy(&bytes).into_owned();
    }
    if let Some(key) = r.stderr_object.take() {
        let bytes = read_object(&store, &key)
            .await
            .with_context(|| format!("deref stderr_object {key}"))?;
        r.stderr = String::from_utf8_lossy(&bytes).into_owned();
    }
    Ok(())
}

/// Inner: pull an Object Store key end-to-end into a Vec<u8>.
/// Sized via `info().size` so the underlying allocation is
/// one-shot — important when an operator runs a 4.6 MB stdout
/// (the original repro for #227) so the read doesn't grow the
/// Vec ten times en route.
async fn read_object(
    store: &async_nats::jetstream::object_store::ObjectStore,
    key: &str,
) -> Result<Vec<u8>> {
    let mut obj = store.get(key).await.context("object_store.get")?;
    let cap = obj.info().size;
    let mut buf = Vec::with_capacity(cap);
    obj.read_to_end(&mut buf).await.context("read_to_end")?;
    Ok(buf)
}

/// Look up the registered job for `r.manifest_id`; if its manifest
/// declares an `inventory:` hint, parse `r.stdout` as JSON and upsert
/// a row into `inventory_facts`. Returns Ok(()) on the "not an
/// inventory job" path (no hint = nothing to do, not an error).
async fn maybe_project_inventory(
    pool: &SqlitePool,
    jobs_kv: &async_nats::jetstream::kv::Store,
    r: &ExecResult,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let Some(manifest_id) = r.manifest_id.as_deref() else {
        return Ok(());
    };
    let entry = match jobs_kv.get(manifest_id).await? {
        Some(b) => b,
        None => return Ok(()), // ad-hoc exec of an unregistered manifest
    };
    let job: Manifest = match serde_json::from_slice(&entry) {
        Ok(j) => j,
        Err(_) => return Ok(()),
    };
    if let Some(hint) = job.inventory.as_ref() {
        return upsert_inventory(pool, r, manifest_id, hint, recorded_at).await;
    }
    Ok(())
}

/// #290 PR-E: look up `r.manifest_id`; if its manifest declares a
/// `check:` hint with `fleet` enabled, read the `status` / `detail`
/// fields out of `r.stdout` and upsert a row into `check_status` for
/// the operator SPA's fleet-wide compliance view. Same "no hint =
/// nothing to do" non-error contract as inventory.
async fn maybe_project_check_status(
    pool: &SqlitePool,
    jobs_kv: &async_nats::jetstream::kv::Store,
    r: &ExecResult,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let Some(manifest_id) = r.manifest_id.as_deref() else {
        return Ok(());
    };
    let entry = match jobs_kv.get(manifest_id).await? {
        Some(b) => b,
        None => return Ok(()),
    };
    let job: Manifest = match serde_json::from_slice(&entry) {
        Ok(j) => j,
        Err(_) => return Ok(()),
    };
    match job.check.as_ref() {
        // `fleet: false` opts a check out of the SPA projection (it
        // still drives the end-user Client App's Health tab).
        Some(hint) if hint.fleet => upsert_check_status(pool, r, hint, recorded_at).await,
        _ => Ok(()),
    }
}

async fn upsert_check_status(
    pool: &SqlitePool,
    r: &ExecResult,
    hint: &CheckHint,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    // Derive (status, detail) the same way the agent's `build_check` /
    // `build_check_failed` do, so the SPA compliance view and the
    // Client App's Health tab never disagree. (A future refactor can
    // hoist this into `kanade-shared` and share it with the agent.)
    let (status, detail): (&str, Option<String>) = if r.exit_code == 0 {
        let facts: serde_json::Value = serde_json::from_str(&r.stdout)
            .with_context(|| format!("check '{}' stdout was not JSON", hint.name))?;
        // Return the `'static` literal (not the slice borrowed from
        // `facts`) so `status` outlives the parsed value; anything
        // outside the four known states normalises to `unknown`.
        let status = match facts.get(&hint.status_field).and_then(|v| v.as_str()) {
            Some("ok") => "ok",
            Some("warn") => "warn",
            Some("fail") => "fail",
            _ => "unknown",
        };
        let detail = facts.get(&hint.detail_field).and_then(json_to_detail);
        (status, detail)
    } else {
        // Crashed before it could report — `unknown`, with the exit
        // code + a stderr snippet (mirrors `build_check_failed`).
        let snippet: String = r.stderr.trim().chars().take(200).collect();
        let detail = if snippet.is_empty() {
            format!("check script exited {}", r.exit_code)
        } else {
            format!("check script exited {}: {snippet}", r.exit_code)
        };
        ("unknown", Some(detail))
    };

    sqlx::query(
        "INSERT INTO check_status (pc_id, check_name, status, detail, recorded_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(pc_id, check_name) DO UPDATE SET
             status      = excluded.status,
             detail      = excluded.detail,
             recorded_at = excluded.recorded_at",
    )
    .bind(&r.pc_id)
    .bind(&hint.name)
    .bind(status)
    .bind(&detail)
    .bind(recorded_at)
    .execute(pool)
    .await
    .with_context(|| format!("upsert check_status for {}/{}", r.pc_id, hint.name))?;

    info!(pc_id = %r.pc_id, check = %hint.name, status, "projected check status");
    Ok(())
}

/// Render a check `detail` field value as a string, mirroring the
/// agent's `check_cache::json_to_detail`: pass non-empty strings,
/// stringify scalars, compact-JSON arrays/objects, drop null/empty.
fn json_to_detail(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        other => Some(other.to_string()),
    }
}

async fn upsert_inventory(
    pool: &SqlitePool,
    r: &ExecResult,
    manifest_id: &str,
    hint: &InventoryHint,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    // Validate the stdout is JSON before we store it — saves the
    // SPA from parsing garbage later.
    let facts: serde_json::Value = serde_json::from_str(&r.stdout)
        .with_context(|| format!("manifest '{manifest_id}' stdout was not JSON"))?;
    let display_json = serde_json::to_string(&hint.display)?;
    let summary_json = hint
        .summary
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    // v0.35 / #93: read the prior `facts_json` BEFORE the upsert
    // overwrites it. We compare scalar fields below (after the
    // upsert) so the history events line up with the new snapshot.
    // Empty Option = first-ever scan for this (pc_id, job_id) →
    // `diff_scalars` will emit `added` events for each declared
    // scalar that's present in the new payload.
    let prior_facts_json: Option<String> =
        if hint.history_scalars.as_ref().is_some_and(|s| !s.is_empty()) {
            sqlx::query_scalar::<_, String>(
                "SELECT facts_json FROM inventory_facts \
              WHERE pc_id = ? AND job_id = ?",
            )
            .bind(&r.pc_id)
            .bind(manifest_id)
            .fetch_optional(pool)
            .await?
        } else {
            None
        };

    // #390: bind recorded_at (RFC 3339) instead of CURRENT_TIMESTAMP
    // so the table keeps one uniform timestamp text format. #398: the
    // value is the message's JetStream publish time, so re-projection
    // reproduces the original arrival stamp.
    sqlx::query(
        "INSERT INTO inventory_facts (
             pc_id, job_id, facts_json, display_json, summary_json,
             collected_at, recorded_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(pc_id, job_id) DO UPDATE SET
             facts_json   = excluded.facts_json,
             display_json = excluded.display_json,
             summary_json = excluded.summary_json,
             collected_at = excluded.collected_at,
             recorded_at  = excluded.recorded_at",
    )
    .bind(&r.pc_id)
    .bind(manifest_id)
    .bind(&r.stdout)
    .bind(display_json)
    .bind(summary_json)
    .bind(r.finished_at)
    .bind(recorded_at)
    .execute(pool)
    .await?;
    info!(
        pc_id = %r.pc_id,
        manifest_id,
        "projected inventory fact",
    );

    // v0.35 / #93: write scalar-field history events into the same
    // `inventory_history` table the explode-array history uses
    // (`identity_json IS NULL` distinguishes the two). Reuses
    // `write_events` so the SPA History tab (#92) renders these
    // rows alongside array-element events without any UI change.
    if let Some(scalars) = hint.history_scalars.as_ref() {
        if !scalars.is_empty() {
            match super::history::diff_scalars(prior_facts_json.as_deref(), &facts, scalars) {
                Ok(events) if !events.is_empty() => {
                    let mut tx = pool.begin().await?;
                    if let Err(e) =
                        super::history::write_events(&mut tx, &r.pc_id, manifest_id, &events).await
                    {
                        warn!(
                            error = %e,
                            pc_id = %r.pc_id,
                            manifest_id,
                            "history_scalars: write_events failed; rolling back",
                        );
                        let _ = tx.rollback().await;
                    } else {
                        tx.commit().await?;
                    }
                }
                Ok(_) => { /* no scalar changed — no events */ }
                Err(e) => warn!(
                    error = %e,
                    pc_id = %r.pc_id,
                    manifest_id,
                    "history_scalars: diff failed; skipping (other projection paths continue)",
                ),
            }
        }
    }

    // v0.31 / #40: replace this PC's rows in each declared `explode`
    // derived table. ensure_table is idempotent so a manifest that
    // gained `explode` between the startup pass and this result
    // still works. Failures per-spec are logged but don't bubble —
    // one bad spec shouldn't take inventory_facts down with it.
    if let Some(specs) = hint.explode.as_ref() {
        for spec in specs {
            // Cached: first delivery per spec pays the CREATE TABLE
            // + CREATE INDEX cost; subsequent results just do an
            // in-memory HashSet lookup. Pre-cache this was N DB
            // round-trips per ExecResult per PC.
            if let Err(e) = super::explode::ensure_table_cached(pool, spec).await {
                warn!(
                    error = %e,
                    pc_id = %r.pc_id,
                    manifest_id,
                    table = %spec.table,
                    "explode: ensure_table failed for this result; skipping",
                );
                continue;
            }
            match super::explode::replace_rows(
                pool,
                spec,
                &r.pc_id,
                manifest_id,
                Some(r.finished_at),
                &facts,
            )
            .await
            {
                Ok(n) => info!(
                    pc_id = %r.pc_id,
                    manifest_id,
                    table = %spec.table,
                    rows = n,
                    "explode: derived rows refreshed",
                ),
                Err(e) => warn!(
                    error = %e,
                    pc_id = %r.pc_id,
                    manifest_id,
                    table = %spec.table,
                    "explode: replace_rows failed for this result",
                ),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
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

    fn sample(result_id: &str, request_id: &str, pc_id: &str, exec_id: Option<&str>) -> ExecResult {
        ExecResult {
            result_id: result_id.into(),
            request_id: request_id.into(),
            exec_id: exec_id.map(str::to_string),
            pc_id: pc_id.into(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap(),
            finished_at: chrono::Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 1).unwrap(),
            stdout_object: None,
            stderr_object: None,
            manifest_id: None,
        }
    }

    #[tokio::test]
    async fn broadcast_results_with_shared_request_id_both_persist() {
        // Issue #19 root cause: pre-v0.29 broadcast Commands had two
        // PCs share one request_id, and the projector's
        // `ON CONFLICT(request_id) DO NOTHING` dropped PC #2's row
        // silently. After the migration to result_id-as-PK both rows
        // must land in the table.
        let pool = fresh_pool().await;
        let a = sample("res-a", "req-shared", "pc-1", Some("exec-1"));
        let b = sample("res-b", "req-shared", "pc-2", Some("exec-1"));
        assert!(
            insert_result(&pool, &a, &a.stable_result_id(), chrono::Utc::now())
                .await
                .unwrap()
        );
        assert!(
            insert_result(&pool, &b, &b.stable_result_id(), chrono::Utc::now())
                .await
                .unwrap()
        );
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM execution_results WHERE request_id = ?")
                .bind("req-shared")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count.0, 2,
            "both per-PC rows should persist under the new result_id PK",
        );
    }

    #[tokio::test]
    async fn duplicate_result_id_is_skipped_and_signaled_false() {
        // JetStream redelivers messages whose ack timed out. The
        // projector relies on `Ok(false)` to skip the counter bump on
        // re-runs — otherwise success_count would over-count.
        let pool = fresh_pool().await;
        let a = sample("res-dup", "req-1", "pc-1", Some("exec-1"));
        let rid = a.stable_result_id();
        assert!(
            insert_result(&pool, &a, &rid, chrono::Utc::now())
                .await
                .unwrap()
        );
        assert!(
            !insert_result(&pool, &a, &rid, chrono::Utc::now())
                .await
                .unwrap(),
            "second insert of same result_id must return false",
        );
    }

    #[tokio::test]
    async fn legacy_payload_redelivery_dedupes_via_stable_uuid() {
        // Gemini #65 medium fix: a legacy ExecResult (no result_id
        // field) re-delivered by JetStream after ack timeout must NOT
        // produce two rows. With the deterministic UUIDv5 derivation,
        // both calls resolve to the same id and ON CONFLICT skips
        // the second insert.
        let pool = fresh_pool().await;
        let r = sample("", "req-1", "pc-1", Some("exec-1"));
        let id1 = r.stable_result_id();
        let id2 = r.stable_result_id();
        assert_eq!(id1, id2, "stable id must be deterministic across calls");
        assert!(
            insert_result(&pool, &r, &id1, chrono::Utc::now())
                .await
                .unwrap()
        );
        assert!(
            !insert_result(&pool, &r, &id2, chrono::Utc::now())
                .await
                .unwrap(),
            "legacy redelivery should be deduped, not double-counted",
        );
    }

    #[tokio::test]
    async fn bump_exec_counters_increments_and_completes() {
        // Set up an executions row with target_count = 2, then bump
        // for one success + one failure → status flips to 'completed'
        // and the counters reflect both results.
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO executions (
                 exec_id, job_id, version, initiated_by, target_count, status
             ) VALUES ('exec-1', 'job-1', '1.0.0', 'tester', 2, 'pending')",
        )
        .execute(&pool)
        .await
        .unwrap();

        bump_exec_counters(&pool, "exec-1", 0).await.unwrap();
        bump_exec_counters(&pool, "exec-1", 7).await.unwrap();

        let row: (i64, i64, String) = sqlx::query_as(
            "SELECT success_count, failure_count, status FROM executions WHERE exec_id = ?",
        )
        .bind("exec-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 1, "one success");
        assert_eq!(row.1, 1, "one failure");
        assert_eq!(row.2, "completed", "status flips when count == target");
    }

    #[tokio::test]
    async fn bump_exec_counters_promotes_pending_to_running_partway() {
        // target_count = 3, one result in → should be 'running', not
        // 'completed' (that's the partial-fan-out case the SPA's
        // Activity Running tab will care about).
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO executions (
                 exec_id, job_id, version, initiated_by, target_count, status
             ) VALUES ('exec-2', 'job-1', '1.0.0', 'tester', 3, 'pending')",
        )
        .execute(&pool)
        .await
        .unwrap();
        bump_exec_counters(&pool, "exec-2", 0).await.unwrap();
        let row: (String,) = sqlx::query_as("SELECT status FROM executions WHERE exec_id = ?")
            .bind("exec-2")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, "running");
    }

    #[tokio::test]
    async fn reaped_placeholder_is_overwritten_by_late_result() {
        // #332 / gemini review: a row the cleanup task reaped
        // (finished_at set, reaped = 1, sentinel exit_code) must still
        // accept the REAL ExecResult if it arrives late (outage heal /
        // a job that ran past the 24 h reap window). Without the
        // `OR reaped = 1` disjunct the genuine output is silently lost.
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, reaped)
             VALUES ('res-late', 'req-1', 'pc-1', -1, '', '[backend: reaped …]',
                     datetime('now', '-2 days'), datetime('now', '-1 day'), 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut r = sample("res-late", "req-1", "pc-1", Some("exec-1"));
        r.exit_code = 0;
        r.stdout = "real output".into();
        let affected = insert_result(&pool, &r, "res-late", chrono::Utc::now())
            .await
            .unwrap();
        assert!(
            affected,
            "late real result must overwrite the reaped placeholder",
        );

        let row: (i64, String, i64) = sqlx::query_as(
            "SELECT exit_code, stdout, reaped FROM execution_results WHERE result_id = ?",
        )
        .bind("res-late")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 0, "exit_code replaced with the real value");
        assert_eq!(row.1, "real output", "stdout replaced");
        assert_eq!(row.2, 0, "reaped flag cleared on overwrite");

        // The row is now genuinely finished (reaped = 0); a JetStream
        // redelivery of the same result must no-op, not re-bump.
        let again = insert_result(&pool, &r, "res-late", chrono::Utc::now())
            .await
            .unwrap();
        assert!(
            !again,
            "redelivery after the real result must not re-update"
        );
    }

    /// #398: the caller-supplied `recorded_at` (the message's JetStream
    /// publish time in production) is what lands in the row — NOT some
    /// internal `Utc::now()`. This is what makes a -WipeDb
    /// re-projection reproduce the original arrival times.
    #[tokio::test]
    async fn recorded_at_is_the_supplied_publish_time() {
        let pool = fresh_pool().await;
        let r = sample("res-pub", "req-1", "pc-1", Some("exec-1"));
        let publish = chrono::Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 2).unwrap();
        assert!(insert_result(&pool, &r, "res-pub", publish).await.unwrap());
        let stored: (chrono::DateTime<chrono::Utc>,) =
            sqlx::query_as("SELECT recorded_at FROM execution_results WHERE result_id = ?")
                .bind("res-pub")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored.0, publish,
            "recorded_at must be the supplied publish time, byte-stable across re-projection",
        );
    }

    // ---- #290 PR-E: check_status compliance projection ----

    fn check_hint(name: &str, status_field: &str) -> CheckHint {
        CheckHint {
            name: name.into(),
            status_field: status_field.into(),
            detail_field: "detail".into(),
            troubleshoot: None,
            fleet: true,
        }
    }

    #[tokio::test]
    async fn check_status_upsert_projects_then_overwrites_in_place() {
        let pool = fresh_pool().await;
        let hint = check_hint("bitlocker", "status");
        let mut r = sample("res-c", "req-c", "pc-1", None);
        r.stdout = r#"{"status":"warn","detail":"D: off"}"#.into();
        let at = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 0, 0, 0).unwrap();
        upsert_check_status(&pool, &r, &hint, at).await.unwrap();

        let row: (String, String, Option<String>) =
            sqlx::query_as("SELECT pc_id, status, detail FROM check_status WHERE check_name = ?")
                .bind("bitlocker")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("pc-1".into(), "warn".into(), Some("D: off".into())));

        // A later run for the same (pc, check) replaces the row, not
        // appends — the table holds the latest status, not a series.
        r.stdout = r#"{"status":"ok","detail":"all protected"}"#.into();
        upsert_check_status(&pool, &r, &hint, at).await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM check_status")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
        let status: (String,) = sqlx::query_as(
            "SELECT status FROM check_status WHERE pc_id = 'pc-1' AND check_name = 'bitlocker'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status.0, "ok");
    }

    #[tokio::test]
    async fn check_status_normalises_unknown_status_and_honours_custom_fields() {
        let pool = fresh_pool().await;
        let hint = check_hint("patch", "compliance");
        let mut r = sample("res-c2", "req-c2", "pc-2", None);
        // Unrecognised status value → stored as `unknown`; the operator's
        // custom status/detail field names are honoured.
        r.stdout = r#"{"compliance":"green","summary":"12 patched"}"#.into();
        let hint = CheckHint {
            detail_field: "summary".into(),
            ..hint
        };
        upsert_check_status(&pool, &r, &hint, chrono::Utc::now())
            .await
            .unwrap();
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT status, detail FROM check_status WHERE pc_id = 'pc-2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("unknown".into(), Some("12 patched".into())));
    }

    #[tokio::test]
    async fn check_status_crash_projects_unknown_with_stderr() {
        let pool = fresh_pool().await;
        let hint = check_hint("bitlocker", "status");
        let mut r = sample("res-c3", "req-c3", "pc-3", None);
        // Script crashed before printing JSON.
        r.exit_code = 1;
        r.stderr = "Get-CimInstance: access denied".into();
        r.stdout = String::new();
        upsert_check_status(&pool, &r, &hint, chrono::Utc::now())
            .await
            .unwrap();
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT status, detail FROM check_status WHERE pc_id = 'pc-3'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "unknown");
        let detail = row.1.unwrap();
        assert!(detail.contains("exited 1"), "detail: {detail}");
        assert!(detail.contains("access denied"), "detail: {detail}");
    }

    #[tokio::test]
    async fn check_status_renders_non_string_detail_as_compact_json() {
        let pool = fresh_pool().await;
        let hint = check_hint("vols", "status");
        let mut r = sample("res-c4", "req-c4", "pc-4", None);
        // A non-string `detail` (array) is rendered as compact JSON,
        // matching the agent's `json_to_detail`, not dropped to NULL.
        r.stdout = r#"{"status":"warn","detail":["C:","D:"]}"#.into();
        upsert_check_status(&pool, &r, &hint, chrono::Utc::now())
            .await
            .unwrap();
        let row: (String, Option<String>) =
            sqlx::query_as("SELECT status, detail FROM check_status WHERE pc_id = 'pc-4'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("warn".into(), Some(r#"["C:","D:"]"#.into())));
    }
}
