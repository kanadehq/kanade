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
///   1. Insert each `ExecResult` into `deployment_results`. ON CONFLICT
///      DO NOTHING so a redelivery doesn't duplicate rows.
///   2. v0.15: if the result carries a `manifest_id` AND a job
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
                if let Err(e) = insert_result(&pool, &r).await {
                    warn!(error = %e, request_id = %r.request_id, "insert result failed");
                } else {
                    info!(request_id = %r.request_id, pc_id = %r.pc_id, exit_code = r.exit_code, "projected result");
                }
                if r.exit_code == 0 {
                    if let Err(e) = maybe_project_inventory(&pool, &jobs_kv, &r).await {
                        warn!(error = ?e, request_id = %r.request_id, "inventory fact projection failed");
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

async fn insert_result(pool: &SqlitePool, r: &ExecResult) -> Result<()> {
    sqlx::query(
        "INSERT INTO deployment_results (
             request_id, pc_id, exit_code, stdout, stderr, started_at, finished_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(request_id) DO NOTHING",
    )
    .bind(&r.request_id)
    .bind(&r.pc_id)
    .bind(r.exit_code as i64)
    .bind(&r.stdout)
    .bind(&r.stderr)
    .bind(r.started_at)
    .bind(r.finished_at)
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
        None => return Ok(()), // ad-hoc deploy of an unregistered manifest
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
