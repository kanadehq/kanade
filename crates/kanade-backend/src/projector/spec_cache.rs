//! v0.35 / #88: in-memory cache of every registered manifest's
//! [`ExplodeSpec`] list, kept fresh by a KV `watch_all()` on
//! [`BUCKET_JOBS`].
//!
//! The HTTP search endpoint (`GET /api/inventory/{manifest_id}/
//! search/{field}`) used to call `kv.get(manifest_id)` on every
//! request to resolve the spec — which was fine for occasional
//! curl traffic but becomes ~30 ms of latency per keystroke once
//! the SPA Software page (#87) makes search interactive. This
//! module replaces that round-trip with an `Arc<RwLock<HashMap>>`
//! lookup and trusts the KV watcher to invalidate stale entries
//! within 1 s of any `kanade job create` write.
//!
//! Wire shape:
//!
//! * Key   — `manifest_id` (e.g. `"inventory-sw"`).
//! * Value — the manifest's `hint.explode` list, or empty when
//!   the manifest has no `explode:` block. Storing empty avoids
//!   re-fetching on every "is this a search-eligible manifest"
//!   miss check.
//!
//! Concurrency:
//!
//! * `RwLock` because reads vastly outnumber writes (writes come
//!   only from KV watcher; reads come from every search request).
//! * The watcher task is a single tokio task spawned at startup;
//!   no cross-watcher coordination needed.
//! * If the watcher exits (NATS reconnect glitch, etc.) the cache
//!   keeps serving — entries will be stale until the watcher is
//!   restarted, but the next `load_explode_spec` falls back to
//!   the KV path on a miss anyway.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv::Operation};
use futures::StreamExt;
use kanade_shared::kv::BUCKET_JOBS;
use kanade_shared::manifest::{ExplodeSpec, Manifest};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Thread-safe `manifest_id → Vec<ExplodeSpec>` cache. Constructed
/// once in `main.rs`, cloned (cheaply — it's an `Arc`) into
/// [`AppState`] and the watcher task.
///
/// [`AppState`]: crate::api::AppState
#[derive(Clone, Default)]
pub struct ExplodeSpecCache {
    inner: Arc<RwLock<HashMap<String, Vec<ExplodeSpec>>>>,
}

impl ExplodeSpecCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read path. Returns `Some(spec)` if `manifest_id` is cached
    /// AND has an explode spec for `field`, otherwise `None`.
    /// `None` lets the caller fall back to the KV fetch path for
    /// either cause (miss vs unknown field) — the distinction
    /// doesn't matter to the slow-path code.
    pub async fn get(&self, manifest_id: &str, field: &str) -> Option<ExplodeSpec> {
        let guard = self.inner.read().await;
        guard
            .get(manifest_id)?
            .iter()
            .find(|s| s.field == field)
            .cloned()
    }

    /// Replace the entry for `manifest_id` with `specs`. Used both
    /// by the cold-cache fallback in `load_explode_spec` (after a
    /// successful KV fetch) and by the KV watcher on every
    /// `Operation::Put`.
    pub async fn insert(&self, manifest_id: String, specs: Vec<ExplodeSpec>) {
        let mut guard = self.inner.write().await;
        guard.insert(manifest_id, specs);
    }

    /// Drop the entry for `manifest_id`. Used by the KV watcher
    /// on `Operation::Delete` / `Operation::Purge` so a job that
    /// gets unregistered immediately stops returning cached specs.
    pub async fn drop_manifest(&self, manifest_id: &str) {
        let mut guard = self.inner.write().await;
        guard.remove(manifest_id);
    }

    /// Count cached entries — used by the watcher's startup log so
    /// operators can confirm prewarm picked up the expected number.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }
}

/// Walk every key in `BUCKET_JOBS` once at startup, decode each
/// `Manifest`, and seed the cache with its `hint.explode` list.
/// Mirrors the prewarm pattern `projector::explode::
/// ensure_tables_for_jobs` already runs at boot — keeps the cold
/// p99 from spiking on the first batch of search requests.
pub async fn prewarm(cache: &ExplodeSpecCache, jetstream: &jetstream::Context) -> Result<usize> {
    let kv = jetstream
        .get_key_value(BUCKET_JOBS)
        .await
        .with_context(|| format!("get KV {BUCKET_JOBS} for prewarm"))?;
    let mut keys = match kv.keys().await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "spec_cache prewarm: keys() failed; will rely on watcher + miss path");
            return Ok(0);
        }
    };
    let mut count = 0usize;
    while let Some(key) = keys.next().await {
        let key = match key {
            Ok(k) => k,
            Err(e) => {
                warn!(error = %e, "spec_cache prewarm: key iter error; skipping");
                continue;
            }
        };
        let entry = match kv.get(&key).await {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, key = %key, "spec_cache prewarm: kv.get failed; skipping");
                continue;
            }
        };
        let manifest: Manifest = match serde_json::from_slice(&entry) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, key = %key, "spec_cache prewarm: decode manifest failed; skipping");
                continue;
            }
        };
        let specs = manifest
            .inventory
            .as_ref()
            .and_then(|h| h.explode.clone())
            .unwrap_or_default();
        cache.insert(manifest.id, specs).await;
        count += 1;
    }
    Ok(count)
}

/// Long-running task that tails `BUCKET_JOBS` and keeps the cache
/// in sync with every manifest write. Spawn-and-forget at startup;
/// exits only on a hard NATS error (caller logs and the cache
/// keeps serving stale data + miss-path fallback).
pub async fn run(cache: ExplodeSpecCache, jetstream: jetstream::Context) -> Result<()> {
    let kv = jetstream
        .get_key_value(BUCKET_JOBS)
        .await
        .with_context(|| format!("get KV {BUCKET_JOBS} for watcher"))?;
    let mut watcher = kv
        .watch_all()
        .await
        .context("kv watch_all on BUCKET_JOBS")?;
    // Hoist the await out of the `info!` arg list — keeping a
    // `.await` inside the macro borrows `format_args!`'s internal
    // non-Send temporary across the await point, which makes the
    // surrounding `tokio::spawn` future fail its Send bound.
    let cached = cache.len().await;
    info!(cached, "explode spec cache watcher started");
    while let Some(entry) = watcher.next().await {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "spec_cache watch: entry error; continuing");
                continue;
            }
        };
        match entry.operation {
            Operation::Put => {
                let manifest: Manifest = match serde_json::from_slice(&entry.value) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(error = %e, key = %entry.key, "spec_cache watch: decode failed");
                        continue;
                    }
                };
                let specs = manifest
                    .inventory
                    .as_ref()
                    .and_then(|h| h.explode.clone())
                    .unwrap_or_default();
                debug!(
                    manifest_id = %manifest.id,
                    n_specs = specs.len(),
                    "spec_cache watch: refresh",
                );
                cache.insert(manifest.id, specs).await;
            }
            Operation::Delete | Operation::Purge => {
                debug!(manifest_id = %entry.key, "spec_cache watch: drop");
                cache.drop_manifest(&entry.key).await;
            }
        }
    }
    // Gemini #120 review: returning an error here surfaces the
    // watcher-stream-ended state to the `tokio::spawn` supervisor
    // in main.rs, which logs it. The cache itself keeps serving
    // its current entries plus the cold-miss KV fallback path, so
    // the failure is non-fatal — but pretending it didn't happen
    // by `std::future::pending::<()>()`-ing would leave the cache
    // silently stale on every subsequent `kanade job create`.
    Err(anyhow::anyhow!(
        "BUCKET_JOBS watch_all stream ended; cache invalidation halted"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::manifest::{ExplodeColumn, ExplodeSpec};

    fn sample_spec(field: &str) -> ExplodeSpec {
        ExplodeSpec {
            field: field.into(),
            table: format!("inventory_test_{field}"),
            primary_key: vec!["name".into()],
            columns: vec![ExplodeColumn {
                field: "name".into(),
                kind: Some("text".into()),
                index: false,
            }],
            track_history: false,
        }
    }

    #[tokio::test]
    async fn miss_returns_none() {
        let cache = ExplodeSpecCache::new();
        assert!(cache.get("inventory-sw", "apps").await.is_none());
    }

    #[tokio::test]
    async fn insert_then_get_returns_spec() {
        let cache = ExplodeSpecCache::new();
        cache
            .insert("inventory-sw".into(), vec![sample_spec("apps")])
            .await;
        let hit = cache.get("inventory-sw", "apps").await.expect("cache hit");
        assert_eq!(hit.field, "apps");
        assert_eq!(hit.table, "inventory_test_apps");
    }

    #[tokio::test]
    async fn insert_replaces_prior_entry() {
        // Watcher receives two Puts for the same manifest — the
        // second wins. Validates against an operator who edits the
        // explode spec via `kanade job create` and re-registers.
        let cache = ExplodeSpecCache::new();
        cache
            .insert("inventory-sw".into(), vec![sample_spec("apps")])
            .await;
        cache
            .insert("inventory-sw".into(), vec![sample_spec("services")])
            .await;
        assert!(cache.get("inventory-sw", "apps").await.is_none());
        assert!(cache.get("inventory-sw", "services").await.is_some());
    }

    #[tokio::test]
    async fn drop_removes_all_fields_for_manifest() {
        let cache = ExplodeSpecCache::new();
        cache
            .insert(
                "inventory-sw".into(),
                vec![sample_spec("apps"), sample_spec("services")],
            )
            .await;
        cache.drop_manifest("inventory-sw").await;
        assert!(cache.get("inventory-sw", "apps").await.is_none());
        assert!(cache.get("inventory-sw", "services").await.is_none());
    }

    #[tokio::test]
    async fn unknown_field_on_known_manifest_returns_none() {
        let cache = ExplodeSpecCache::new();
        cache
            .insert("inventory-sw".into(), vec![sample_spec("apps")])
            .await;
        assert!(cache.get("inventory-sw", "services").await.is_none());
    }
}
