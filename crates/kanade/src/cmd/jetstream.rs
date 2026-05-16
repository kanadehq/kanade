use anyhow::Result;
use async_nats::jetstream;
use clap::{Args, Subcommand};
use kanade_shared::bootstrap::ensure_jetstream_resources;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_SCHEDULES,
    BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, STREAM_AUDIT,
    STREAM_DEPLOY, STREAM_EVENTS, STREAM_INVENTORY, STREAM_RESULTS,
};

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
    // Single source of truth lives in kanade-shared so the backend's
    // startup-time auto-bootstrap and this operator-facing command
    // create the exact same set of resources.
    ensure_jetstream_resources(&js).await?;

    println!("jetstream setup complete:");
    println!(
        "  streams       : {STREAM_INVENTORY}, {STREAM_RESULTS}, {STREAM_DEPLOY}, {STREAM_EVENTS}, {STREAM_AUDIT}"
    );
    println!(
        "  KV            : {BUCKET_SCRIPT_CURRENT}, {BUCKET_SCRIPT_STATUS}, {BUCKET_AGENTS_STATE}, {BUCKET_AGENT_CONFIG}, {BUCKET_AGENT_GROUPS}, {BUCKET_SCHEDULES}"
    );
    println!("  object stores : {OBJECT_AGENT_RELEASES}");
    Ok(())
}

async fn status(js: jetstream::Context) -> Result<()> {
    println!("streams:");
    for name in [
        STREAM_INVENTORY,
        STREAM_RESULTS,
        STREAM_DEPLOY,
        STREAM_AUDIT,
    ] {
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
        BUCKET_AGENT_CONFIG,
    ] {
        match js.get_key_value(bucket).await {
            Ok(_) => println!("  {bucket}: OK"),
            Err(_) => println!("  {bucket}: NOT FOUND"),
        }
    }
    println!("object stores:");
    for name in [OBJECT_AGENT_RELEASES] {
        match js.get_object_store(name).await {
            Ok(_) => println!("  {name}: OK"),
            Err(_) => println!("  {name}: NOT FOUND"),
        }
    }
    Ok(())
}
