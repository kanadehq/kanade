use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::ExecResult;
use kanade_shared::kv::{BUCKET_JOBS, STREAM_RESULTS};
use kanade_shared::manifest::{InventoryHint, Manifest};
use sqlx::SqlitePool;
use tracing::{info, warn};

const CONSUMER_NAME: &str = "backend_results_projector";

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
        match serde_json::from_slice::<ExecResult>(&msg.payload) {
            Ok(r) => {
                // Resolve once and reuse for log + insert. For v0.29+
                // agents this is just `r.result_id`; for legacy
                // payloads this is the deterministic UUIDv5 derived
                // from (request_id, pc_id).
                let resolved_id = r.stable_result_id();
                match insert_result(&pool, &r, &resolved_id).await {
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
                    if let Err(e) = maybe_project_inventory(&pool, &jobs_kv, &r).await {
                        warn!(error = ?e, result_id = %resolved_id, "inventory fact projection failed");
                    }
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

/// Insert one ExecResult; return `Ok(true)` when the row was inserted,
/// `Ok(false)` when ON CONFLICT skipped it (= redelivery from
/// JetStream's ack-timeout retry of a result we already projected).
/// The boolean is what gates the `executions` counter bump above —
/// without this the counters would over-count on every redelivery.
async fn insert_result(pool: &SqlitePool, r: &ExecResult, result_id: &str) -> Result<bool> {
    // `result_id` is pre-resolved by the caller via
    // `r.stable_result_id()`: agent-supplied for v0.29+ payloads,
    // deterministic UUIDv5 from (request_id, pc_id) for legacy.
    // Determinism is load-bearing: JetStream redelivery of the same
    // legacy payload must hash to the same id so ON CONFLICT skips
    // it, otherwise the executions counters double-count.
    let rows = sqlx::query(
        "INSERT INTO execution_results (
             result_id, request_id, exec_id, pc_id, exit_code,
             stdout, stderr, started_at, finished_at, job_id
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(result_id) DO NOTHING",
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

/// Look up the registered job for `r.manifest_id`; if its manifest
/// declares an `inventory:` hint, parse `r.stdout` as JSON and upsert
/// a row into `inventory_facts`. Returns Ok(()) on the "not an
/// inventory job" path (no hint = nothing to do, not an error).
async fn maybe_project_inventory(
    pool: &SqlitePool,
    jobs_kv: &async_nats::jetstream::kv::Store,
    r: &ExecResult,
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
        return upsert_inventory(pool, r, manifest_id, hint).await;
    }
    Ok(())
}

async fn upsert_inventory(
    pool: &SqlitePool,
    r: &ExecResult,
    manifest_id: &str,
    hint: &InventoryHint,
) -> Result<()> {
    // Validate the stdout is JSON before we store it — saves the
    // SPA from parsing garbage later.
    let _facts: serde_json::Value = serde_json::from_str(&r.stdout)
        .with_context(|| format!("manifest '{manifest_id}' stdout was not JSON"))?;
    let display_json = serde_json::to_string(&hint.display)?;
    let summary_json = hint
        .summary
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    sqlx::query(
        "INSERT INTO inventory_facts (
             pc_id, job_id, facts_json, display_json, summary_json,
             collected_at, recorded_at
         ) VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(pc_id, job_id) DO UPDATE SET
             facts_json   = excluded.facts_json,
             display_json = excluded.display_json,
             summary_json = excluded.summary_json,
             collected_at = excluded.collected_at,
             recorded_at  = CURRENT_TIMESTAMP",
    )
    .bind(&r.pc_id)
    .bind(manifest_id)
    .bind(&r.stdout)
    .bind(display_json)
    .bind(summary_json)
    .bind(r.finished_at)
    .execute(pool)
    .await?;
    info!(
        pc_id = %r.pc_id,
        manifest_id,
        "projected inventory fact",
    );
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
            insert_result(&pool, &a, &a.stable_result_id())
                .await
                .unwrap()
        );
        assert!(
            insert_result(&pool, &b, &b.stable_result_id())
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
        assert!(insert_result(&pool, &a, &rid).await.unwrap());
        assert!(
            !insert_result(&pool, &a, &rid).await.unwrap(),
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
        assert!(insert_result(&pool, &r, &id1).await.unwrap());
        assert!(
            !insert_result(&pool, &r, &id2).await.unwrap(),
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
}
