//! `kanade app` — manage the generic app-package Object Store
//! (`OBJECT_APP_PACKAGES`, #207).
//!
//! Sibling of `kanade agent publish`: same NATS-direct shape (no
//! backend HTTP hop), different bucket. `agent publish` covers the
//! agent's own self-update binary, this covers everything else
//! operators install on endpoints — kanade-client, kanade-backend,
//! Webex / Teams / vendor MSIs, etc.
//!
//! See `kanade-shared::kv::OBJECT_APP_PACKAGES` for the bucket-
//! level design notes. Object key shape is `<name>/<version>`;
//! operator picks `<name>` once per package family and `<version>`
//! per release.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures::StreamExt;
use kanade_shared::kv::OBJECT_APP_PACKAGES;
use tracing::info;

#[derive(Args, Debug)]
pub struct AppArgs {
    #[command(subcommand)]
    pub sub: AppSub,
}

#[derive(Subcommand, Debug)]
pub enum AppSub {
    /// Upload a binary / installer to the app_packages Object Store
    /// under `<name>/<version>`. Mirrors `kanade agent publish` —
    /// goes straight at NATS, no backend HTTP round-trip.
    ///
    /// Operators pick `<name>` once per package family
    /// (e.g. `kanade-client`, `kanade-backend`, `webex-meetings`).
    /// `<version>` defaults to the binary's embedded VERSIONINFO
    /// (same pelite extraction as `kanade agent publish`) — pass
    /// `--version` to override (vendor MSIs / non-PE binaries need
    /// the explicit label).
    Publish {
        /// Package family name. Slash-free, ASCII-printable; see
        /// `kanade-backend::api::app_packages::validate_segment`
        /// for the full set of restrictions the HTTP side enforces.
        name: String,
        /// Path to the binary to upload.
        binary: PathBuf,
        /// Version label. When omitted, extracted from the binary's
        /// embedded VERSIONINFO (Windows PE built with `winres` —
        /// every kanade-* binary qualifies). Required for binaries
        /// without VERSIONINFO (most vendor installers) — fails fast
        /// rather than silently uploading under an empty version.
        #[arg(long)]
        version: Option<String>,
    },
    /// List every `<name>/<version>` row in the bucket — size +
    /// digest + last-modified.
    List,
    /// Delete a single package version. No-op + clear message when
    /// the key isn't present (idempotent re-runs are fine).
    Delete { name: String, version: String },
}

pub async fn execute(client: async_nats::Client, args: AppArgs) -> Result<()> {
    match args.sub {
        AppSub::Publish {
            name,
            binary,
            version,
        } => publish(client, name, binary, version).await,
        AppSub::List => list(client).await,
        AppSub::Delete { name, version } => delete(client, name, version).await,
    }
}

/// Mirror of `kanade-backend::api::app_packages::validate_segment`
/// so the CLI rejects the same shapes the HTTP endpoint would
/// (avoids a confusing "upload succeeded; download 400s" loop).
/// Keeping a separate copy is fine — the constraint set is small,
/// stable, and lives outside the wire crate today.
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
    binary: PathBuf,
    version: Option<String>,
) -> Result<()> {
    validate_segment("name", &name)?;

    // #261: default version to the binary's embedded VERSIONINFO —
    // same pelite extractor that `kanade agent publish` uses. Avoids
    // the operator typing the version twice (once in the build, once
    // here) for kanade-* binaries. Explicit `--version` overrides for
    // non-PE inputs (MSIs, scripts) where extraction returns None.
    //
    // The extractor needs the full bytes in memory, but the upload
    // path below streams from disk to keep RSS bounded for 256 MB
    // app packages. So when extraction is needed we slurp once for
    // the header read, then re-open the file as a stream for put().
    let resolved_version = match version {
        Some(v) => v,
        None => {
            let bytes = tokio::fs::read(&binary)
                .await
                .with_context(|| format!("read {binary:?}"))?;
            kanade_shared::exe_version::extract_pe_version(&bytes).with_context(|| {
                format!(
                    "no --version given and couldn't extract VERSIONINFO from {binary:?} \
                     (Windows PE built with `winres`? otherwise pass `--version <label>`)"
                )
            })?
        }
    };
    validate_segment("version", &resolved_version)?;
    let version = resolved_version;

    // Stream from disk instead of slurping (Gemini #222 MED).
    // App packages can hit 256 MB — buffering the whole binary
    // would peak the CLI's RSS unnecessarily, and `Object Store::put`
    // already takes `&mut impl AsyncRead`, so streaming is the
    // natural shape.
    let mut file = tokio::fs::File::open(&binary)
        .await
        .with_context(|| format!("open {binary:?}"))?;
    info!(name, version, "uploading app package");

    let js = async_nats::jetstream::new(client);
    let store = js
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .with_context(|| {
            format!("object store '{OBJECT_APP_PACKAGES}' missing — run `kanade jetstream setup`")
        })?;
    let key = format!("{name}/{version}");
    let meta = store
        .put(key.as_str(), &mut file)
        .await
        .context("object_store.put")?;
    info!(name, version, size = meta.size, digest = ?meta.digest, "app package uploaded");

    println!("published: {key}");
    println!("  object_store : {OBJECT_APP_PACKAGES}/{key}");
    println!("  size         : {} bytes", meta.size);
    if let Some(d) = meta.digest.as_deref() {
        println!("  digest       : {d}");
    }
    Ok(())
}

async fn list(client: async_nats::Client) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let store = js
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .with_context(|| {
            format!("object store '{OBJECT_APP_PACKAGES}' missing — run `kanade jetstream setup`")
        })?;
    let mut list = store.list().await.context("object_store.list")?;
    let mut rows: Vec<Row> = Vec::new();
    while let Some(item) = list.next().await {
        let meta = item.context("list app packages")?;
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
        println!("(no app packages)");
        return Ok(());
    }
    for row in rows {
        let dgst = row.digest.as_deref().unwrap_or("—");
        let modt = row.modified.as_deref().unwrap_or("—");
        // TSV: `<key>\t<size>\t<modified>\t<digest>` — shell-pipe
        // friendly. SPA Apps page (#218) renders the same fields.
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
    let js = async_nats::jetstream::new(client);
    let store = js
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .with_context(|| {
            format!("object store '{OBJECT_APP_PACKAGES}' missing — run `kanade jetstream setup`")
        })?;
    let key = format!("{name}/{version}");
    match store.delete(key.as_str()).await {
        Ok(()) => {
            info!(%key, "app package deleted");
            println!("deleted: {key}");
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
