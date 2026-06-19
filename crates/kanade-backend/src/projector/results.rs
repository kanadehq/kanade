use std::sync::Arc;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::ExecResult;
use kanade_shared::kv::{BUCKET_JOBS, OBJECT_RESULT_OUTPUT, STREAM_RESULTS};
use kanade_shared::manifest::{CheckHint, InventoryHint, Manifest};
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

use super::spec_cache::ExplodeSpecCache;

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
pub async fn run(
    js: jetstream::Context,
    pool: SqlitePool,
    jobs_cache: ExplodeSpecCache,
) -> Result<()> {
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
                match project_result(&pool, &r, &resolved_id, recorded_at).await {
                    Ok(true) => {
                        debug!(
                            result_id = %resolved_id,
                            request_id = %r.request_id,
                            exec_id = ?r.exec_id,
                            pc_id = %r.pc_id,
                            exit_code = r.exit_code,
                            "projected result",
                        );
                    }
                    Ok(false) => {
                        debug!(
                            result_id = %resolved_id,
                            "duplicate result (ON CONFLICT) — skipping counter bump",
                        );
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            result_id = %resolved_id,
                            "result projection failed — skipping ack so JetStream redelivers",
                        );
                        // #484: skip the ack so JetStream redelivers.
                        // A transient failure (SQLite busy past the
                        // 30 s busy_timeout under fleet write
                        // contention, #411) would otherwise ack the
                        // message and permanently lose the ExecResult
                        // — the in-flight row stays 'running' until
                        // the 24 h reaper stamps it dead. Redelivery
                        // is safe: insert_result dedups via ON
                        // CONFLICT, and the events / obs_events
                        // projectors already use this exact
                        // skip-ack-on-failure discipline.
                        continue;
                    }
                }
                if r.exit_code == 0 {
                    if let Err(e) =
                        maybe_project_inventory(&pool, &jobs_cache, &jobs_kv, &r, recorded_at).await
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
                if let Err(e) =
                    maybe_project_check_status(&js, &pool, &jobs_cache, &jobs_kv, &r, recorded_at)
                        .await
                {
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

/// Project one ExecResult: the `execution_results` row write and the
/// `executions` counter bump commit in a single transaction, and the
/// caller acks only on `Ok`. Without the transaction (PR #537 review,
/// CodeRabbit): if `insert_result` succeeds but `bump_exec_counters`
/// fails transiently, the redelivery hits the ON CONFLICT dedup
/// (`Ok(false)`) and skips the bump — leaving `executions.status` /
/// `success_count` permanently out of sync with the projected row.
/// Atomicity makes the redelivery repair both or neither.
///
/// Returns `insert_result`'s freshness bool (true = this call
/// projected the result, false = redelivery no-op).
async fn project_result(
    pool: &SqlitePool,
    r: &ExecResult,
    result_id: &str,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let mut tx = pool.begin().await.context("begin result projection tx")?;
    // #682 Stage 2: was this row already reaped? The cleanup reaper now
    // counts a reaped orphan as a failure against `executions`
    // (bump_exec_counters), so a *late* real result overwriting that
    // reaped placeholder must NOT bump again — otherwise the exec's
    // counters double-count that PC. Captured before the upsert clears
    // `reaped` back to 0.
    // COALESCE(reaped, 0): `reaped` is `NOT NULL DEFAULT 0` today so it
    // can't actually be NULL, but the COALESCE keeps the Option<i64>
    // decode robust if that constraint ever changes (gemini #694). The
    // outer Option is row presence: None = no row yet (ExecResult-first
    // race) → not reaped → bump proceeds.
    let was_reaped: Option<i64> =
        sqlx::query_scalar("SELECT COALESCE(reaped, 0) FROM execution_results WHERE result_id = ?")
            .bind(result_id)
            .fetch_optional(&mut *tx)
            .await?;
    let fresh = insert_result(&mut *tx, r, result_id, recorded_at).await?;
    // Only bump exec counters on a fresh insert. Redeliveries
    // (`false`) must not double-count — JetStream redelivers on ack
    // timeout, and `executions.success_count` is an unconditional
    // `+= 1`. A reaped-placeholder overwrite (was_reaped) is also
    // skipped: the reaper already counted that row at reap time.
    if fresh && was_reaped != Some(1) {
        if let Some(exec_id) = r.exec_id.as_deref() {
            bump_exec_counters(&mut *tx, exec_id, r.exit_code).await?;
        }
    }
    tx.commit().await.context("commit result projection tx")?;
    Ok(fresh)
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
async fn insert_result<'e, E>(
    executor: E,
    r: &ExecResult,
    result_id: &str,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
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
    .execute(executor)
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
pub(crate) async fn bump_exec_counters<'e, E>(
    executor: E,
    exec_id: &str,
    exit_code: i32,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
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
    .execute(executor)
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

/// #488: manifest lookup for the per-result hint projections. Cache
/// hit = zero broker round-trips (the BUCKET_JOBS watcher keeps the
/// cache fresh within ~1 s of any `kanade job create`); a miss falls
/// back to one `jobs_kv.get` and warms the cache for the rest of the
/// burst. Pre-fix every ExecResult paid TWO `jobs_kv.get` round
/// trips (one per hint), which capped the strictly-serial projector
/// at ~250–500 results/s during fleet-wide bursts.
async fn lookup_manifest(
    cache: &ExplodeSpecCache,
    jobs_kv: &async_nats::jetstream::kv::Store,
    manifest_id: &str,
) -> Result<Option<Arc<Manifest>>> {
    if let Some(m) = cache.manifest(manifest_id).await {
        return Ok(Some(m));
    }
    let entry = match jobs_kv.get(manifest_id).await? {
        Some(b) => b,
        None => return Ok(None), // ad-hoc exec of an unregistered manifest
    };
    let job: Manifest = match serde_json::from_slice(&entry) {
        Ok(j) => j,
        Err(e) => {
            // Don't swallow silently — a corrupted catalog entry
            // would otherwise be indistinguishable from "no such
            // job" (review PR #553).
            warn!(error = %e, manifest_id, "lookup_manifest: KV entry failed to decode");
            return Ok(None);
        }
    };
    Ok(Some(cache.insert_manifest(job).await))
}

/// Look up the registered job for `r.manifest_id`; if its manifest
/// declares an `inventory:` hint, parse `r.stdout` as JSON and upsert
/// a row into `inventory_facts`. Returns Ok(()) on the "not an
/// inventory job" path (no hint = nothing to do, not an error).
async fn maybe_project_inventory(
    pool: &SqlitePool,
    cache: &ExplodeSpecCache,
    jobs_kv: &async_nats::jetstream::kv::Store,
    r: &ExecResult,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let Some(manifest_id) = r.manifest_id.as_deref() else {
        return Ok(());
    };
    let Some(job) = lookup_manifest(cache, jobs_kv, manifest_id).await? else {
        return Ok(());
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
    js: &jetstream::Context,
    pool: &SqlitePool,
    cache: &ExplodeSpecCache,
    jobs_kv: &async_nats::jetstream::kv::Store,
    r: &ExecResult,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let Some(manifest_id) = r.manifest_id.as_deref() else {
        return Ok(());
    };
    let Some(job) = lookup_manifest(cache, jobs_kv, manifest_id).await? else {
        return Ok(());
    };
    let Some(hint) = job.check.as_ref().filter(|h| h.fleet) else {
        // `fleet: false` opts a check out of the SPA projection (it still
        // drives the end-user Client App's Health tab).
        return Ok(());
    };
    let proj = upsert_check_status(pool, r, hint, recorded_at).await?;

    // Compliance auto-notification (PR-B): fire once on a transition into an
    // alert status, for live results only. `fresh` also folds in "this is
    // the newest result" so an out-of-order (stale) result — whose upsert
    // was a no-op — doesn't alert. Best-effort: `fire` logs and swallows its
    // own errors so a publish hiccup never wedges the projector.
    //
    // The prior-read + upsert in `upsert_check_status` aren't one
    // transaction, and `proj.status` is the *incoming* status (not the
    // post-upsert table value). Both are safe because this consumer is
    // serial (durable pull, one message at a time): no two projections of
    // the same PC+check interleave, and a stale/out-of-order result is
    // rejected here by `is_newest` before its `proj.status` is ever used.
    // If concurrency is ever added, wrap the read+upsert in a transaction
    // with `RETURNING` to capture the prior atomically.
    if let Some(alert) = &hint.alert {
        let is_newest = proj
            .prior
            .as_ref()
            .is_none_or(|(_, prev_at)| recorded_at >= *prev_at);
        let fresh = is_newest && super::check_alert::is_fresh(recorded_at, chrono::Utc::now());
        let prior_status = proj.prior.as_ref().map(|(s, _)| s.as_str());
        if super::check_alert::should_fire(alert, prior_status, proj.status, fresh) {
            super::check_alert::fire(
                js,
                pool,
                r,
                hint,
                alert,
                proj.status,
                proj.detail.as_deref().unwrap_or(""),
            )
            .await;
        }
    }
    Ok(())
}

/// What [`upsert_check_status`] hands back so [`maybe_project_check_status`]
/// can decide whether to fire a compliance alert.
struct CheckProjection {
    /// The status just projected (`ok`/`warn`/`fail`/`unknown`).
    status: &'static str,
    /// The one-line detail just projected.
    detail: Option<String>,
    /// The prior `(status, recorded_at)` for this PC+check — `None` when
    /// this was the first projection, or when the check has no `alert:`
    /// block (in which case it isn't read at all).
    prior: Option<(String, chrono::DateTime<chrono::Utc>)>,
}

async fn upsert_check_status(
    pool: &SqlitePool,
    r: &ExecResult,
    hint: &CheckHint,
    recorded_at: chrono::DateTime<chrono::Utc>,
) -> Result<CheckProjection> {
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

    // Read the prior projected state BEFORE the upsert so a compliance
    // alert can fire on a *transition* (e.g. ok → fail), not on every
    // poll. `None` = first projection for this PC+check. Only loaded when
    // the check actually carries an `alert:` block (the common case has
    // none, so we skip the extra read).
    let prior: Option<(String, chrono::DateTime<chrono::Utc>)> = if hint.alert.is_some() {
        sqlx::query_as(
            "SELECT status, recorded_at FROM check_status WHERE pc_id = ? AND check_name = ?",
        )
        .bind(&r.pc_id)
        .bind(&hint.name)
        .fetch_optional(pool)
        .await
        .with_context(|| format!("read prior check_status for {}/{}", r.pc_id, hint.name))?
    } else {
        None
    };

    sqlx::query(
        "INSERT INTO check_status (pc_id, check_name, label, status, detail, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(pc_id, check_name) DO UPDATE SET
             label       = excluded.label,
             status      = excluded.status,
             detail      = excluded.detail,
             recorded_at = excluded.recorded_at
         WHERE excluded.recorded_at >= check_status.recorded_at",
    )
    .bind(&r.pc_id)
    .bind(&hint.name)
    .bind(&hint.label)
    .bind(status)
    .bind(&detail)
    .bind(recorded_at)
    .execute(pool)
    .await
    .with_context(|| format!("upsert check_status for {}/{}", r.pc_id, hint.name))?;

    debug!(pc_id = %r.pc_id, check = %hint.name, status, "projected check status");

    Ok(CheckProjection {
        status,
        detail,
        prior,
    })
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
    let rows = sqlx::query(
        "INSERT INTO inventory_facts (
             pc_id, job_id, facts_json, display_json, summary_json,
             collected_at, recorded_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(pc_id, job_id) DO UPDATE SET
             facts_json   = excluded.facts_json,
             display_json = excluded.display_json,
             summary_json = excluded.summary_json,
             collected_at = excluded.collected_at,
             recorded_at  = excluded.recorded_at
         WHERE excluded.recorded_at >= inventory_facts.recorded_at",
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
    // #503: rows_affected == 0 means the recency guard rejected a
    // stale redelivery. Everything downstream of the snapshot write
    // must no-op too — most importantly the history diff: at this
    // point `prior_facts_json` holds the NEWER facts still in the
    // table and `facts` the OLDER replayed payload, so diffing them
    // would write reversed newer→older history events for a
    // snapshot that never changed (PR #569 review, claude).
    let stale_replay = rows.rows_affected() == 0;
    if stale_replay {
        debug!(
            pc_id = %r.pc_id,
            manifest_id,
            "stale inventory replay rejected by recency guard; skipping history diff",
        );
        return Ok(());
    }
    debug!(
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
                Ok(n) => debug!(
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
            collect_object: None,
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

    #[tokio::test]
    async fn late_result_on_reaped_row_does_not_rebump_executions() {
        // #682 Stage 2: the reaper already counts a reaped orphan as a
        // failure against `executions`. If the REAL result then arrives
        // late and overwrites the reaped placeholder, project_result must
        // NOT bump again — otherwise that PC is counted twice. The output
        // is still corrected (handled by insert_result); only the counter
        // bump is suppressed.
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO executions (
                 exec_id, job_id, version, initiated_by, target_count, status
             ) VALUES ('exec-r', 'job-1', '1.0.0', 'tester', 1, 'running')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Reaper already settled this PC as a failure.
        bump_exec_counters(&pool, "exec-r", -1).await.unwrap();
        // The reaped placeholder row it left behind.
        sqlx::query(
            "INSERT INTO execution_results
                (result_id, request_id, exec_id, pc_id, exit_code, stdout, stderr,
                 started_at, finished_at, reaped)
             VALUES ('res-r', 'req-1', 'exec-r', 'pc-1', -1, '', '[backend: reaped …]',
                     datetime('now', '-2 days'), datetime('now', '-1 day'), 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Late REAL result (success) overwrites the placeholder.
        let mut r = sample("res-r", "req-1", "pc-1", Some("exec-r"));
        r.exit_code = 0;
        r.stdout = "real output".into();
        let fresh = project_result(&pool, &r, "res-r", chrono::Utc::now())
            .await
            .unwrap();
        assert!(fresh, "the overwrite is a fresh transition");

        // Output corrected, but counters NOT double-bumped: still the
        // single failure the reaper recorded (not failure=1 + success=1).
        let er: (i64, String) =
            sqlx::query_as("SELECT exit_code, stdout FROM execution_results WHERE result_id = ?")
                .bind("res-r")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(er.0, 0, "real exit_code lands");
        assert_eq!(er.1, "real output", "real stdout lands");
        let ex: (i64, i64) =
            sqlx::query_as("SELECT success_count, failure_count FROM executions WHERE exec_id = ?")
                .bind("exec-r")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ex.0, 0, "no extra success counted");
        assert_eq!(
            ex.1, 1,
            "still exactly one (the reaper's) failure — not double-counted"
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
            label: None,
            status_field: status_field.into(),
            detail_field: "detail".into(),
            troubleshoot: None,
            fleet: true,
            alert: None,
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
    async fn stale_check_replay_does_not_regress_status() {
        // #503: an out-of-order JetStream redelivery (older
        // recorded_at) must NOT roll the latest status back.
        let pool = fresh_pool().await;
        let hint = check_hint("bitlocker", "status");
        let mut r = sample("res-r1", "req-r1", "pc-1", None);
        let newer = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let older = newer - chrono::Duration::seconds(1);

        r.stdout = r#"{"status":"ok","detail":"all protected"}"#.into();
        upsert_check_status(&pool, &r, &hint, newer).await.unwrap();

        // Stale replay carrying the OLD failing state.
        r.stdout = r#"{"status":"fail","detail":"D: off"}"#.into();
        upsert_check_status(&pool, &r, &hint, older).await.unwrap();

        let status: (String,) = sqlx::query_as(
            "SELECT status FROM check_status WHERE pc_id = 'pc-1' AND check_name = 'bitlocker'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status.0, "ok", "stale replay must not regress the row");
    }

    #[tokio::test]
    async fn stale_inventory_replay_does_not_regress_facts() {
        // #503: same guard on inventory_facts, including that the
        // recency-rejected replay returns Ok (no redelivery storm).
        let pool = fresh_pool().await;
        let hint = InventoryHint {
            display: vec![],
            summary: None,
            explode: None,
            history_scalars: None,
        };
        let mut r = sample("res-i1", "req-i1", "pc-1", None);
        let newer = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let older = newer - chrono::Duration::seconds(1);

        r.stdout = r#"{"ram_gb": 32}"#.into();
        upsert_inventory(&pool, &r, "inv-test", &hint, newer)
            .await
            .unwrap();
        r.stdout = r#"{"ram_gb": 16}"#.into();
        upsert_inventory(&pool, &r, "inv-test", &hint, older)
            .await
            .unwrap();

        let facts: (String,) = sqlx::query_as(
            "SELECT facts_json FROM inventory_facts WHERE pc_id = 'pc-1' AND job_id = 'inv-test'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            facts.0.contains("32"),
            "stale replay must not regress facts: {}",
            facts.0
        );
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
    async fn check_status_projects_label_when_hint_carries_one() {
        let pool = fresh_pool().await;
        // Operator-authored display title flows from the hint into the
        // row; a hint without one leaves label NULL (UI falls back to
        // the slug).
        let hint = CheckHint {
            label: Some("ディスクの空き容量".into()),
            ..check_hint("disk_space", "status")
        };
        let mut r = sample("res-lbl", "req-lbl", "pc-9", None);
        r.stdout = r#"{"status":"ok","detail":"20% free"}"#.into();
        upsert_check_status(&pool, &r, &hint, chrono::Utc::now())
            .await
            .unwrap();
        let labeled: Option<String> =
            sqlx::query_scalar("SELECT label FROM check_status WHERE check_name = 'disk_space'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(labeled.as_deref(), Some("ディスクの空き容量"));

        let bare = check_hint("firewall", "status");
        let mut r2 = sample("res-lbl2", "req-lbl2", "pc-9", None);
        r2.stdout = r#"{"status":"ok"}"#.into();
        upsert_check_status(&pool, &r2, &bare, chrono::Utc::now())
            .await
            .unwrap();
        let none: Option<String> =
            sqlx::query_scalar("SELECT label FROM check_status WHERE check_name = 'firewall'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn alert_check_captures_prior_status_across_projections() {
        use kanade_shared::manifest::{CheckAlert, CheckAlertStatus};
        // An `alert:`-enabled check makes `upsert_check_status` read the
        // prior (status, recorded_at) before the upsert — the input the
        // transition decision in `maybe_project_check_status` keys off.
        // This locks in that DB-integration so a refactor can't silently
        // break the prior-capture (the firing decision itself is unit-
        // tested in `check_alert`).
        let pool = fresh_pool().await;
        let hint = CheckHint {
            alert: Some(CheckAlert {
                on: vec![CheckAlertStatus::Fail],
                notify_user: true,
                notify_groups: vec![],
                priority: kanade_shared::ipc::notifications::NotificationPriority::Warn,
                require_ack: false,
                toast: true,
                title: "t".into(),
                body: None,
            }),
            ..check_hint("bitlocker", "status")
        };
        let t0 = chrono::Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

        // First-ever projection: no prior.
        let mut r = sample("a1", "q1", "pc-a", None);
        r.stdout = r#"{"status":"ok"}"#.into();
        let p1 = upsert_check_status(&pool, &r, &hint, t0).await.unwrap();
        assert!(p1.prior.is_none(), "first projection ⇒ no prior");
        assert_eq!(p1.status, "ok");

        // ok → fail: prior is the just-stored "ok".
        r.stdout = r#"{"status":"fail"}"#.into();
        let p2 = upsert_check_status(&pool, &r, &hint, t0 + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(p2.prior.as_ref().map(|(s, _)| s.as_str()), Some("ok"));
        assert_eq!(p2.status, "fail");

        // Stays fail: prior is "fail" (so the decision layer won't re-fire).
        let p3 = upsert_check_status(&pool, &r, &hint, t0 + chrono::Duration::seconds(2))
            .await
            .unwrap();
        assert_eq!(p3.prior.as_ref().map(|(s, _)| s.as_str()), Some("fail"));
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
