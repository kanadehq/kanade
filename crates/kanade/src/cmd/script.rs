//! `kanade script` — manage the manifest-script Object Store
//! (`OBJECT_SCRIPTS`, #211).
//!
//! Sibling of `kanade app` — same NATS-direct shape, different
//! bucket. This one holds the PowerShell / shell / etc. bodies
//! that manifests reference via `execute.script_object` (#213
//! schema, #214 agent fetch). Bodies are bounded at 4 MB
//! (vs `app_packages`'s 256 MB) — scripts are KB-to-MB text, not
//! installer binaries.
//!
//! Object key shape is `<name>/<version>` — same as `app`. For
//! manifest-driven scripts `<name>` is conventionally the manifest
//! id and `<version>` matches the manifest version, but the bucket
//! imposes no policy (operator-uploaded ad-hoc scripts can use any
//! pair they like).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures::StreamExt;
use kanade_shared::kv::OBJECT_SCRIPTS;
use tracing::info;

#[derive(Args, Debug)]
pub struct ScriptArgs {
    #[command(subcommand)]
    pub sub: ScriptSub,
}

#[derive(Subcommand, Debug)]
pub enum ScriptSub {
    /// Upload a script body to the scripts Object Store under
    /// `<name>/<version>`. Use this for bodies referenced by a
    /// manifest's `execute.script_object` field — agents fetch +
    /// sha-verify at exec time (#214).
    Publish {
        /// Script "name" — typically the referencing manifest's id.
        name: String,
        /// Script "version" — typically the referencing manifest's
        /// version. Operator picks any scheme; the bucket just stores
        /// the pair as the key.
        version: String,
        /// Path to the script file (.ps1 / .sh / .py / …).
        file: PathBuf,
    },
    /// List every `<name>/<version>` row in the bucket — size +
    /// digest + last-modified.
    List,
    /// Delete a single script version. No-op + clear message when
    /// the key isn't present.
    Delete { name: String, version: String },
}

pub async fn execute(client: async_nats::Client, args: ScriptArgs) -> Result<()> {
    match args.sub {
        ScriptSub::Publish {
            name,
            version,
            file,
        } => publish(client, name, version, file).await,
        ScriptSub::List => list(client).await,
        ScriptSub::Delete { name, version } => delete(client, name, version).await,
    }
}

/// Mirror of `kanade-backend::api::script_objects::validate_segment`.
/// See the matching note in `cmd::app::validate_segment` — keeping
/// a CLI-side copy fails fast on a typo before round-tripping to
/// the bucket.
fn validate_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must be non-empty");
    }
    if value.contains('/') {
        bail!("{label} must not contain '/'");
    }
    for c in value.chars() {
        if !c.is_ascii() {
            bail!("{label} must be ASCII-printable (rejected non-ASCII {c:?})");
        }
        if c.is_ascii_control() {
            bail!("{label} must not contain control characters");
        }
        if c == '"' || c == '\\' {
            bail!("{label} must not contain '\"' or '\\\\'");
        }
    }
    Ok(())
}

async fn publish(
    client: async_nats::Client,
    name: String,
    version: String,
    file: PathBuf,
) -> Result<()> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;

    // Stream from disk (Gemini #222 MED) — bodies cap at 4 MB so
    // the OOM angle is mild here, but matching the `app publish`
    // shape keeps the two commands symmetrical for review.
    let mut reader = tokio::fs::File::open(&file)
        .await
        .with_context(|| format!("open {file:?}"))?;
    info!(name, version, "uploading script object");

    let js = async_nats::jetstream::new(client.clone());
    let store = js.get_object_store(OBJECT_SCRIPTS).await.with_context(|| {
        format!("object store '{OBJECT_SCRIPTS}' missing — run `kanade jetstream setup`")
    })?;
    let key = format!("{name}/{version}");
    let meta = store
        .put(key.as_str(), &mut reader)
        .await
        .context("object_store.put")?;
    info!(name, version, size = meta.size, digest = ?meta.digest, "script object uploaded");

    println!("published: {key}");
    println!("  object_store : {OBJECT_SCRIPTS}/{key}");
    println!("  size         : {} bytes", meta.size);
    if let Some(d) = meta.digest.as_deref() {
        println!("  digest       : {d}");
    }
    println!();
    println!("Reference from a manifest with:");
    println!("  execute:");
    println!("    shell: powershell");
    println!("    script_object: {key}");
    println!("    timeout: 600s");

    crate::audit::record(
        &client,
        "script_object_publish",
        Some(&key),
        serde_json::json!({ "size": meta.size, "digest": meta.digest }),
    )
    .await;
    Ok(())
}

async fn list(client: async_nats::Client) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let store = js.get_object_store(OBJECT_SCRIPTS).await.with_context(|| {
        format!("object store '{OBJECT_SCRIPTS}' missing — run `kanade jetstream setup`")
    })?;
    let mut list = store.list().await.context("object_store.list")?;
    let mut rows: Vec<Row> = Vec::new();
    while let Some(item) = list.next().await {
        let meta = item.context("list script objects")?;
        rows.push(Row {
            key: meta.name,
            size: meta.size,
            digest: meta.digest,
            modified: meta
                .modified
                .and_then(|t| chrono::DateTime::from_timestamp(t.unix_timestamp(), t.nanosecond()))
                .map(|d| d.to_rfc3339()),
        });
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    if rows.is_empty() {
        println!("(no script objects)");
        return Ok(());
    }
    for row in rows {
        let dgst = row.digest.as_deref().unwrap_or("—");
        let modt = row.modified.as_deref().unwrap_or("—");
        println!("{}\t{}\t{}\t{}", row.key, row.size, modt, dgst);
    }
    Ok(())
}

struct Row {
    key: String,
    size: usize,
    digest: Option<String>,
    modified: Option<String>,
}

async fn delete(client: async_nats::Client, name: String, version: String) -> Result<()> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;
    let js = async_nats::jetstream::new(client.clone());
    let store = js.get_object_store(OBJECT_SCRIPTS).await.with_context(|| {
        format!("object store '{OBJECT_SCRIPTS}' missing — run `kanade jetstream setup`")
    })?;
    let key = format!("{name}/{version}");
    match store.delete(key.as_str()).await {
        Ok(()) => {
            info!(%key, "script object deleted");
            println!("deleted: {key}");
            crate::audit::record(
                &client,
                "script_object_delete",
                Some(&key),
                serde_json::json!({}),
            )
            .await;
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("no objects") {
                println!("not present: {key} (idempotent no-op)");
                Ok(())
            } else {
                Err(e).with_context(|| format!("object_store.delete {key}"))
            }
        }
    }
}
