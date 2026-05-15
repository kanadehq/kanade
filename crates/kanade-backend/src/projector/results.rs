use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::ExecResult;
use kanade_shared::kv::STREAM_RESULTS;
use sqlx::SqlitePool;
use tracing::{info, warn};

const CONSUMER_NAME: &str = "backend_results_projector";

/// Consume the RESULTS stream and insert each `ExecResult` into
/// `deployment_results`. ON CONFLICT DO NOTHING so a redelivery doesn't
/// duplicate rows.
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
