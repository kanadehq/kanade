use anyhow::{Context, Result, anyhow, bail};
use async_nats::jetstream;
use clap::{Args, Subcommand, ValueEnum};
use kanade_shared::bootstrap::ensure_jetstream_resources;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_JOBS, BUCKET_SCHEDULES,
    BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, STREAM_AUDIT,
    STREAM_EVENTS, STREAM_EXEC, STREAM_INVENTORY, STREAM_RESULTS,
};
use serde::Deserialize;

#[derive(Args, Debug)]
pub struct JetstreamArgs {
    #[command(subcommand)]
    pub sub: JetstreamSub,
}

#[derive(Subcommand, Debug)]
pub enum JetstreamSub {
    /// Create every stream + KV bucket the agent expects (idempotent).
    Setup,
    /// Print current state of streams + KV buckets + object stores, as the
    /// backend sees them. Goes through the backend API (needs
    /// KANADE_AUTH_TOKEN), not NATS — unlike the other `jetstream` subcommands.
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
    /// this command lists will be deleted. Without this flag the
    /// command prints what would be wiped and exits non-zero.
    #[arg(long)]
    pub yes: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum ResourceKind {
    Stream,
    Bucket,
    Store,
}

/// The NATS-backed subcommands. `status` is HTTP and is routed to
/// [`status`] before any broker connection is made.
pub async fn execute(client: async_nats::Client, args: JetstreamArgs) -> Result<()> {
    let js = jetstream::new(client);
    match args.sub {
        JetstreamSub::Setup => setup(js).await,
        JetstreamSub::Status => unreachable!("status is dispatched over HTTP"),
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

/// One resource in the backend's `GET /api/jetstream/status` snapshot. Usage
/// numbers are optional so a missing value is never shown as a made-up zero.
#[derive(Deserialize, Debug)]
struct Probe {
    name: String,
    exists: bool,
    #[serde(default)]
    bytes: Option<u64>,
    #[serde(default)]
    messages: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct Snapshot {
    streams: Vec<Probe>,
    kv_buckets: Vec<Probe>,
    object_stores: Vec<Probe>,
}

fn render_status(snap: &Snapshot) -> String {
    use std::fmt::Write;
    fn num(v: Option<u64>) -> String {
        v.map_or_else(|| "?".to_string(), |n| n.to_string())
    }
    let mut out = String::from("streams:\n");
    for p in &snap.streams {
        if p.exists {
            let _ = writeln!(
                out,
                "  {}: messages={}, bytes={}",
                p.name,
                num(p.messages),
                num(p.bytes)
            );
        } else {
            let _ = writeln!(out, "  {}: NOT FOUND", p.name);
        }
    }
    for (title, probes) in [
        ("KV buckets:", &snap.kv_buckets),
        ("object stores:", &snap.object_stores),
    ] {
        let _ = writeln!(out, "{title}");
        for p in probes {
            let state = if p.exists { "OK" } else { "NOT FOUND" };
            let _ = writeln!(out, "  {}: {state}", p.name);
        }
    }
    out
}

pub async fn status(backend_url: &str) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    let url = format!("{base}/api/jetstream/status");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("jetstream status failed: {status} — {body}");
    }
    let snap: Snapshot = resp
        .json()
        .await
        .with_context(|| format!("parse JSON response from GET {url}"))?;
    print!("{}", render_status(&snap));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    const SNAP: &str = r#"{
        "streams":[{"name":"EXEC","exists":true,"bytes":10,"max_bytes":100,"messages":3},
                   {"name":"AUDIT","exists":false}],
        "kv_buckets":[{"name":"jobs","exists":true,"bytes":1,"messages":1}],
        "object_stores":[{"name":"agent_releases","exists":false}]
    }"#;

    #[test]
    fn render_keeps_the_three_sections_and_not_found() {
        let snap: Snapshot = serde_json::from_str(SNAP).unwrap();
        assert_eq!(
            render_status(&snap),
            "streams:\n  EXEC: messages=3, bytes=10\n  AUDIT: NOT FOUND\n\
             KV buckets:\n  jobs: OK\nobject stores:\n  agent_releases: NOT FOUND\n"
        );
    }

    #[test]
    fn missing_usage_is_not_shown_as_zero() {
        let snap: Snapshot = serde_json::from_str(
            r#"{"streams":[{"name":"EXEC","exists":true}],"kv_buckets":[],"object_stores":[]}"#,
        )
        .unwrap();
        assert!(render_status(&snap).contains("EXEC: messages=?, bytes=?"));
    }

    #[tokio::test]
    async fn status_sends_get_to_the_snapshot_route() {
        let (base, log) = fake_backend(vec![(200, SNAP)]).await;
        status(&format!("{base}/")).await.unwrap();
        let got = seen(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/jetstream/status");
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn auth_failures_report_status_and_body() {
        for (code, body) in [(401, "bad token"), (403, "feature disabled")] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let err = status(&base).await.unwrap_err().to_string();
            assert!(err.contains("jetstream status failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
        }
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let (base, _log) = fake_backend(vec![(200, "{")]).await;
        let err = status(&base).await.unwrap_err().to_string();
        assert!(err.contains("parse JSON response"), "{err}");
    }

    #[tokio::test]
    async fn connection_failure_names_the_request() {
        let err = status("http://127.0.0.1:1").await.unwrap_err().to_string();
        assert!(
            err.contains("GET http://127.0.0.1:1/api/jetstream/status"),
            "{err}"
        );
    }
}
