use anyhow::{Result, anyhow, bail};
use async_nats::jetstream;
use clap::{Args, Subcommand, ValueEnum};
use kanade_shared::bootstrap::ensure_jetstream_resources;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_JOBS, BUCKET_SCHEDULES,
    BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, STREAM_AUDIT,
    STREAM_EVENTS, STREAM_EXEC, STREAM_INVENTORY, STREAM_RESULTS,
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
    /// Delete a single JetStream resource by kind + name. Useful as
    /// a surgical recovery when one stream's config drifted on the
    /// broker and `setup` keeps failing on it — v0.25.0's
    /// create-or-update bootstrap usually reconciles automatically,
    /// but messages from the old config aren't migrated.
    Delete(DeleteArgs),
    /// Wipe every stream, KV bucket, and object store the fleet uses,
    /// then re-bootstrap from scratch. Destructive — all retained
    /// commands, results, audit history, schedule definitions, job
    /// catalog, group membership, etc. are gone. Intended for dev /
    /// CI; refuses to run without `--yes`.
    Reset(ResetArgs),
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Resource kind to delete.
    #[arg(value_enum)]
    pub kind: ResourceKind,
    /// Resource name (e.g. `EXEC` for a stream, `agent_config` for a
    /// KV bucket, `agent_releases` for an object store).
    pub name: String,
    /// Required acknowledgement that this is destructive. Without
    /// this flag the command prints what *would* be deleted and
    /// exits non-zero.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct ResetArgs {
    /// Required acknowledgement that every stream + bucket + store
    /// listed by `kanade jetstream status` will be deleted. Without
    /// this flag the command prints what would be wiped and exits
    /// non-zero.
    #[arg(long)]
    pub yes: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum ResourceKind {
    Stream,
    Bucket,
    Store,
}

pub async fn execute(client: async_nats::Client, args: JetstreamArgs) -> Result<()> {
    let js = jetstream::new(client);
    match args.sub {
        JetstreamSub::Setup => setup(js).await,
        JetstreamSub::Status => status(js).await,
        JetstreamSub::Delete(d) => delete(js, d).await,
        JetstreamSub::Reset(r) => reset(js, r).await,
    }
}

async fn setup(js: jetstream::Context) -> Result<()> {
    // Single source of truth lives in kanade-shared so the backend's
    // startup-time auto-bootstrap and this operator-facing command
    // create the exact same set of resources.
    ensure_jetstream_resources(&js).await?;

    println!("jetstream setup complete:");
    println!(
        "  streams       : {STREAM_INVENTORY}, {STREAM_RESULTS}, {STREAM_EXEC}, {STREAM_EVENTS}, {STREAM_AUDIT}"
    );
    println!(
        "  KV            : {BUCKET_SCRIPT_CURRENT}, {BUCKET_SCRIPT_STATUS}, {BUCKET_AGENTS_STATE}, {BUCKET_AGENT_CONFIG}, {BUCKET_AGENT_GROUPS}, {BUCKET_SCHEDULES}, {BUCKET_JOBS}"
    );
    println!("  object stores : {OBJECT_AGENT_RELEASES}");
    Ok(())
}

const ALL_STREAMS: &[&str] = &[
    STREAM_INVENTORY,
    STREAM_RESULTS,
    STREAM_EXEC,
    STREAM_EVENTS,
    STREAM_AUDIT,
];
const ALL_BUCKETS: &[&str] = &[
    BUCKET_SCRIPT_CURRENT,
    BUCKET_SCRIPT_STATUS,
    BUCKET_AGENTS_STATE,
    BUCKET_AGENT_CONFIG,
    BUCKET_AGENT_GROUPS,
    BUCKET_SCHEDULES,
    BUCKET_JOBS,
];
const ALL_STORES: &[&str] = &[OBJECT_AGENT_RELEASES];

async fn status(js: jetstream::Context) -> Result<()> {
    println!("streams:");
    for name in ALL_STREAMS {
        match js.get_stream(*name).await {
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
    for bucket in ALL_BUCKETS {
        match js.get_key_value(*bucket).await {
            Ok(_) => println!("  {bucket}: OK"),
            Err(_) => println!("  {bucket}: NOT FOUND"),
        }
    }
    println!("object stores:");
    for name in ALL_STORES {
        match js.get_object_store(*name).await {
            Ok(_) => println!("  {name}: OK"),
            Err(_) => println!("  {name}: NOT FOUND"),
        }
    }
    Ok(())
}

async fn delete(js: jetstream::Context, args: DeleteArgs) -> Result<()> {
    let kind_label = match args.kind {
        ResourceKind::Stream => "stream",
        ResourceKind::Bucket => "KV bucket",
        ResourceKind::Store => "object store",
    };

    if !args.yes {
        println!(
            "would delete {kind_label} {name} (re-run with --yes)",
            name = args.name
        );
        bail!("--yes not supplied");
    }

    match args.kind {
        ResourceKind::Stream => {
            js.delete_stream(&args.name)
                .await
                .map_err(|e| anyhow!("delete_stream {}: {e}", args.name))?;
        }
        ResourceKind::Bucket => {
            js.delete_key_value(&args.name)
                .await
                .map_err(|e| anyhow!("delete_key_value {}: {e}", args.name))?;
        }
        ResourceKind::Store => {
            js.delete_object_store(&args.name)
                .await
                .map_err(|e| anyhow!("delete_object_store {}: {e}", args.name))?;
        }
    }
    println!("deleted {kind_label} {}", args.name);
    Ok(())
}

async fn reset(js: jetstream::Context, args: ResetArgs) -> Result<()> {
    if !args.yes {
        println!("would delete the following resources and re-bootstrap (re-run with --yes):");
        println!("  streams       : {}", ALL_STREAMS.join(", "));
        println!("  KV            : {}", ALL_BUCKETS.join(", "));
        println!("  object stores : {}", ALL_STORES.join(", "));
        bail!("--yes not supplied");
    }

    // Order is intentionally lenient — failures on "NOT FOUND" are
    // expected (partial state from a previous failed bootstrap), so
    // we log and continue rather than abort the wipe halfway.
    for name in ALL_STREAMS {
        match js.delete_stream(*name).await {
            Ok(_) => println!("deleted stream {name}"),
            Err(e) => println!("skip stream {name}: {e}"),
        }
    }
    for bucket in ALL_BUCKETS {
        match js.delete_key_value(*bucket).await {
            Ok(_) => println!("deleted bucket {bucket}"),
            Err(e) => println!("skip bucket {bucket}: {e}"),
        }
    }
    for name in ALL_STORES {
        match js.delete_object_store(*name).await {
            Ok(_) => println!("deleted store {name}"),
            Err(e) => println!("skip store {name}: {e}"),
        }
    }

    println!("re-bootstrapping...");
    ensure_jetstream_resources(&js).await?;
    println!("reset complete.");
    Ok(())
}
