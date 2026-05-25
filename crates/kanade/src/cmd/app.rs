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
    /// (e.g. `kanade-client`, `kanade-backend`, `webex-meetings`)
    /// and `<version>` per release. Both parameters echo back as
    /// the Object Store key, so a typo at submit time is silently
    /// recoverable with a follow-up `kanade app delete`.
    Publish {
        /// Package family name. Slash-free, ASCII-printable; see
        /// `kanade-backend::api::app_packages::validate_segment`
        /// for the full set of restrictions the HTTP side enforces.
        name: String,
        /// Version string (semver / calendar / git sha — operator's
        /// choice). Same character restrictions as `name`.
        version: String,
        /// Path to the binary to upload.
        binary: PathBuf,
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
            version,
            binary,
        } => publish(client, name, version, binary).await,
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
    version: String,
    binary: PathBuf,
) -> Result<()> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;

    let bytes = tokio::fs::read(&binary)
        .await
        .with_context(|| format!("read {binary:?}"))?;
    let size = bytes.len();
    info!(name, version, size, "uploading app package");

    let js = async_nats::jetstream::new(client);
    let store = js
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .with_context(|| {
            format!("object store '{OBJECT_APP_PACKAGES}' missing — run `kanade jetstream setup`")
        })?;
    let key = format!("{name}/{version}");
    let mut cursor = std::io::Cursor::new(bytes);
    let meta = store
        .put(key.as_str(), &mut cursor)
        .await
        .context("object_store.put")?;
    info!(name, version, digest = ?meta.digest, "app package uploaded");

    println!("published: {key}");
    println!("  object_store : {OBJECT_APP_PACKAGES}/{key}");
    println!("  size         : {size} bytes");
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
    let mut rows: Vec<(String, usize, Option<String>)> = Vec::new();
    while let Some(item) = list.next().await {
        let meta = item.context("list app packages")?;
        rows.push((meta.name, meta.size, meta.digest));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        println!("(no app packages)");
        return Ok(());
    }
    for (key, size, digest) in rows {
        let dgst = digest.as_deref().unwrap_or("—");
        println!("{key}\t{size}\t{dgst}");
    }
    Ok(())
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
