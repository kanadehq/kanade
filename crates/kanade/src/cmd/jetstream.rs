use std::time::Duration;

use anyhow::Result;
use async_nats::jetstream::{
    self,
    kv::Config as KvConfig,
    stream::{Config as StreamConfig, DiscardPolicy},
};
use clap::{Args, Subcommand};
use kanade_shared::kv::{
    BUCKET_AGENTS_STATE, BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, STREAM_DEPLOY,
    STREAM_INVENTORY, STREAM_RESULTS,
};
use tracing::info;

#[derive(Args, Debug)]
pub struct JetstreamArgs {
    #[command(subcommand)]
    pub sub: JetstreamSub,
}

#[derive(Subcommand, Debug)]
pub enum JetstreamSub {
    /// Create every stream + KV bucket the agent expects (idempotent).
    Setup,
    /// Print current state of streams + KV buckets.
    Status,
}

pub async fn execute(client: async_nats::Client, args: JetstreamArgs) -> Result<()> {
    let js = jetstream::new(client);
    match args.sub {
        JetstreamSub::Setup => setup(js).await,
        JetstreamSub::Status => status(js).await,
    }
}

async fn setup(js: jetstream::Context) -> Result<()> {
    // INVENTORY — 90-day rolling history (spec §2.3.1).
    js.create_stream(StreamConfig {
        name: STREAM_INVENTORY.into(),
        subjects: vec!["inventory.>".into()],
        max_age: Duration::from_secs(90 * 24 * 60 * 60),
        ..Default::default()
    })
    .await?;
    info!(stream = STREAM_INVENTORY, "ready");

    // RESULTS — 30-day rolling history.
    js.create_stream(StreamConfig {
        name: STREAM_RESULTS.into(),
        subjects: vec!["results.>".into()],
        max_age: Duration::from_secs(30 * 24 * 60 * 60),
        ..Default::default()
    })
    .await?;
    info!(stream = STREAM_RESULTS, "ready");

    // DEPLOY — latest-per-subject only (spec §2.6 Layer 1).
    js.create_stream(StreamConfig {
        name: STREAM_DEPLOY.into(),
        subjects: vec!["commands.deploy.>".into()],
        max_messages_per_subject: 1,
        discard: DiscardPolicy::Old,
        max_age: Duration::from_secs(7 * 24 * 60 * 60),
        ..Default::default()
    })
    .await?;
    info!(stream = STREAM_DEPLOY, "ready");

    // script_current KV — cmd_id → version (spec §2.6 Layer 2).
    js.create_key_value(KvConfig {
        bucket: BUCKET_SCRIPT_CURRENT.into(),
        history: 5,
        ..Default::default()
    })
    .await?;
    info!(bucket = BUCKET_SCRIPT_CURRENT, "ready");

    // script_status KV — cmd_id → ACTIVE / REVOKED.
    js.create_key_value(KvConfig {
        bucket: BUCKET_SCRIPT_STATUS.into(),
        history: 5,
        ..Default::default()
    })
    .await?;
    info!(bucket = BUCKET_SCRIPT_STATUS, "ready");

    // agents_state KV — pc_id → latest hardware snapshot (history=1).
    js.create_key_value(KvConfig {
        bucket: BUCKET_AGENTS_STATE.into(),
        history: 1,
        ..Default::default()
    })
    .await?;
    info!(bucket = BUCKET_AGENTS_STATE, "ready");

    println!("jetstream setup complete:");
    println!("  streams : {STREAM_INVENTORY}, {STREAM_RESULTS}, {STREAM_DEPLOY}");
    println!(
        "  KV      : {BUCKET_SCRIPT_CURRENT}, {BUCKET_SCRIPT_STATUS}, {BUCKET_AGENTS_STATE}"
    );
    Ok(())
}

async fn status(js: jetstream::Context) -> Result<()> {
    println!("streams:");
    for name in [STREAM_INVENTORY, STREAM_RESULTS, STREAM_DEPLOY] {
        match js.get_stream(name).await {
            Ok(mut stream) => match stream.info().await {
                Ok(info) => println!(
                    "  {name}: messages={}, bytes={}",
                    info.state.messages, info.state.bytes
                ),
                Err(e) => println!("  {name}: info error: {e}"),
            },
            Err(_) => println!("  {name}: NOT FOUND"),
        }
    }
    println!("KV buckets:");
    for bucket in [
        BUCKET_SCRIPT_CURRENT,
        BUCKET_SCRIPT_STATUS,
        BUCKET_AGENTS_STATE,
    ] {
        match js.get_key_value(bucket).await {
            Ok(_) => println!("  {bucket}: OK"),
            Err(_) => println!("  {bucket}: NOT FOUND"),
        }
    }
    Ok(())
}
