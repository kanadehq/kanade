//! #1216: object-store metadata index.
//!
//! The four object-store list endpoints (`/api/collect/bundles`,
//! `/api/agents/releases`, `/api/app-packages`, `/api/script-objects`)
//! used to call `ObjectStore::list()` per request — an ephemeral
//! `DeliverPolicy::All` consumer over `$O.<bucket>.M.>`, i.e. a full
//! stream scan (chunk data + delete tombstones included) on every
//! cold page load: 10-30 s on the measured buckets, growing forever
//! with the stream's sequence span.
//!
//! This projector replaces the per-request scan with a SQLite index
//! (`object_store_meta`). One supervised `watch_with_history()` per
//! bucket — `DeliverPolicy::LastPerSubject` over the metadata
//! subjects only, never chunk data — replays the current meta of
//! every object at attach and then streams puts + delete tombstones
//! live. The handlers read the index, so a page load is one local
//! SELECT.
//!
//! Sync coverage:
//!   * backend API publishes/deletes — seen via their meta messages,
//!     AND written through synchronously by the mutation handlers
//!     (see [`apply`]/[`delete_key`]). Write-through exists because
//!     the SPA invalidates + refetches the list immediately after a
//!     successful upload/delete; reading the not-yet-updated index
//!     there would briefly show a stale list (#1216 review).
//!   * direct-NATS writers the backend never sees synchronously
//!     (agent collect uploads, `kanade agent publish`,
//!     `kanade app|script publish/delete`) — watcher only. This is
//!     exactly why write-through at the API handlers alone would not
//!     have been enough either.
//!   * objects deleted while the backend was down — the tombstone is
//!     the LAST message on the meta subject, so the history replay
//!     delivers it and the row is removed.
//!   * not covered: a manual `nats stream purge` on `OBJ_*` (operator
//!     disaster-recovery territory — rebuild by deleting the table's
//!     rows and letting the next attach re-sync).

use std::time::Duration;

use async_nats::jetstream;
use async_nats::jetstream::object_store::ObjectInfo;
use futures::StreamExt;
use kanade_shared::kv::{
    OBJECT_AGENT_RELEASES, OBJECT_APP_PACKAGES, OBJECT_COLLECTIONS, OBJECT_SCRIPTS,
};
use sqlx::SqlitePool;
use tracing::{info, warn};

const REOPEN_BACKOFF: Duration = Duration::from_secs(5);

/// Buckets with a list endpoint reading the index.
pub const BUCKETS: [&str; 4] = [
    OBJECT_COLLECTIONS,
    OBJECT_AGENT_RELEASES,
    OBJECT_APP_PACKAGES,
    OBJECT_SCRIPTS,
];

/// Supervised watch loop for one bucket, forever. Mirrors the
/// `agent_meta` projector: an attach/entry failure reopens after
/// [`REOPEN_BACKOFF`] instead of halting the projection until the
/// next backend restart. `watch_with_history` redelivers the current
/// meta of every object on each (re)attach, so buffered events +
/// replay re-apply idempotently (upsert / delete-by-key).
pub async fn run(pool: SqlitePool, jetstream: jetstream::Context, bucket: &'static str) {
    loop {
        let store = match jetstream.get_object_store(bucket).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, bucket, "object_meta: get_object_store failed; retrying");
                tokio::time::sleep(REOPEN_BACKOFF).await;
                continue;
            }
        };
        let mut watcher = match store.watch_with_history().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, bucket, "object_meta: watch failed; retrying");
                tokio::time::sleep(REOPEN_BACKOFF).await;
                continue;
            }
        };
        info!(bucket, "object_meta projector (re)attached");
        while let Some(item) = watcher.next().await {
            match item {
                Ok(meta) => {
                    if let Err(e) = apply(&pool, bucket, &meta).await {
                        warn!(error = %e, bucket, key = %meta.name, "object_meta: apply failed");
                    }
                }
                Err(e) => {
                    warn!(error = %e, bucket, "object_meta watch: entry error; continuing");
                }
            }
        }
        warn!(bucket, "object_meta watch ended; reopening");
        tokio::time::sleep(REOPEN_BACKOFF).await;
    }
}

/// Apply one meta message: tombstone → drop the row, live meta →
/// upsert. Object Store `delete()` publishes the tombstone as the
/// last message on the meta subject (async-nats sets `deleted: true`
/// with a rollup header), so both paths arrive through this one
/// function.
///
/// Also called by the mutation handlers as write-through (module
/// docs): idempotent with the watcher, so the meta message landing
/// afterwards is a no-op re-apply.
pub async fn apply(pool: &SqlitePool, bucket: &str, meta: &ObjectInfo) -> Result<(), sqlx::Error> {
    if meta.deleted {
        return delete_key(pool, bucket, &meta.name).await;
    }
    let modified = meta.modified.and_then(|t| {
        chrono::DateTime::from_timestamp(t.unix_timestamp(), t.nanosecond()).map(|d| d.to_rfc3339())
    });
    sqlx::query(
        "INSERT INTO object_store_meta (bucket, key, size, digest, modified)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (bucket, key) DO UPDATE SET
             size = excluded.size,
             digest = excluded.digest,
             modified = excluded.modified",
    )
    .bind(bucket)
    .bind(&meta.name)
    .bind(meta.size as i64)
    .bind(&meta.digest)
    .bind(modified)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop one index row. Watcher path: via [`apply`] on a tombstone.
/// Write-through path: called directly by the delete handlers so the
/// SPA's immediate post-delete refetch doesn't see the row (module
/// docs). Idempotent — deleting a missing key is a no-op.
pub async fn delete_key(pool: &SqlitePool, bucket: &str, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM object_store_meta WHERE bucket = ? AND key = ?")
        .bind(bucket)
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// One index row, as the four list handlers consume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaRow {
    pub key: String,
    pub size: i64,
    pub digest: Option<String>,
    pub modified: Option<String>,
}

/// Read every live object in a bucket. Key order is unspecified —
/// each handler sorts for its own page, same as before.
pub async fn list_bucket(pool: &SqlitePool, bucket: &str) -> Result<Vec<MetaRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, i64, Option<String>, Option<String>)>(
        "SELECT key, size, digest, modified FROM object_store_meta WHERE bucket = ?",
    )
    .bind(bucket)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(key, size, digest, modified)| MetaRow {
            key,
            size,
            digest,
            modified,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn meta(name: &str, size: usize, deleted: bool) -> ObjectInfo {
        ObjectInfo {
            name: name.to_string(),
            description: None,
            metadata: Default::default(),
            headers: None,
            options: None,
            bucket: OBJECT_SCRIPTS.to_string(),
            nuid: format!("nuid-{name}"),
            size,
            chunks: 1,
            modified: None,
            digest: Some("SHA-256=abc".to_string()),
            deleted,
        }
    }

    #[tokio::test]
    async fn put_then_list_returns_the_object() {
        let pool = pool().await;
        apply(&pool, OBJECT_SCRIPTS, &meta("a/1.0.0", 42, false))
            .await
            .unwrap();
        let rows = list_bucket(&pool, OBJECT_SCRIPTS).await.unwrap();
        assert_eq!(
            rows,
            vec![MetaRow {
                key: "a/1.0.0".to_string(),
                size: 42,
                digest: Some("SHA-256=abc".to_string()),
                modified: None,
            }],
        );
    }

    #[tokio::test]
    async fn re_put_overwrites_in_place() {
        let pool = pool().await;
        apply(&pool, OBJECT_SCRIPTS, &meta("a/1.0.0", 42, false))
            .await
            .unwrap();
        apply(&pool, OBJECT_SCRIPTS, &meta("a/1.0.0", 99, false))
            .await
            .unwrap();
        let rows = list_bucket(&pool, OBJECT_SCRIPTS).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size, 99);
    }

    #[tokio::test]
    async fn tombstone_removes_the_row() {
        let pool = pool().await;
        apply(&pool, OBJECT_SCRIPTS, &meta("a/1.0.0", 42, false))
            .await
            .unwrap();
        apply(&pool, OBJECT_SCRIPTS, &meta("a/1.0.0", 0, true))
            .await
            .unwrap();
        assert!(list_bucket(&pool, OBJECT_SCRIPTS).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn buckets_are_isolated() {
        let pool = pool().await;
        apply(&pool, OBJECT_SCRIPTS, &meta("a/1.0.0", 1, false))
            .await
            .unwrap();
        apply(&pool, OBJECT_APP_PACKAGES, &meta("a/1.0.0", 2, false))
            .await
            .unwrap();
        assert_eq!(list_bucket(&pool, OBJECT_SCRIPTS).await.unwrap()[0].size, 1);
        assert_eq!(
            list_bucket(&pool, OBJECT_APP_PACKAGES).await.unwrap()[0].size,
            2,
        );
        // Deleting from one bucket must not touch the other's row
        // for the same key.
        apply(&pool, OBJECT_SCRIPTS, &meta("a/1.0.0", 0, true))
            .await
            .unwrap();
        assert!(list_bucket(&pool, OBJECT_SCRIPTS).await.unwrap().is_empty());
        assert_eq!(
            list_bucket(&pool, OBJECT_APP_PACKAGES).await.unwrap().len(),
            1
        );
    }
}
