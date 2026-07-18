//! Projects the `agent_meta` NATS KV bucket into the `agent_meta` SQLite
//! table (#1051) so the Agents list can render metadata columns and
//! filter by metadata in SQL.
//!
//! The KV bucket is the source of truth — the SPA `PUT
//! /api/agents/{pc_id}/meta` and the `kanade meta` CLI write it. This
//! table is a **read-only** rebuild owned by a single supervised task
//! ([`run`]): it reconciles the full KV snapshot after every watch
//! (re)attach and then tails live updates. Mirrors the `schedules`
//! projector rather than `spec_cache`, because async-nats `watch_all`
//! has no initial delivery (#911) and the projection must survive a
//! watch-stream end without a backend restart.
//!
//! On `-WipeDb` the table is recreated empty by the migration and the
//! first reconcile repopulates it from KV — nothing is lost because KV
//! holds the durable copy.

use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv::Operation};
use futures::StreamExt;
use kanade_shared::kv::BUCKET_AGENT_META;
use kanade_shared::wire::AgentMeta;
use sqlx::SqlitePool;
use tracing::{info, warn};

/// Delay between watch reopens after the stream ends or a (re)attach
/// fails — matches the `schedules` / freeze watchers.
const REOPEN_BACKOFF: Duration = Duration::from_secs(5);

/// Full reconcile: rebuild the whole table from the current KV snapshot.
/// Returns the number of `(pc, key)` rows written.
///
/// Run after every watch (re)attach — async-nats `watch_all` delivers
/// only *new* updates, no initial snapshot (#911), so without a reconcile
/// a boot (or a reconnect) would miss every existing key. The rebuild
/// runs in one transaction (clear + re-insert) so a reader never observes
/// the table mid-wipe.
///
/// **Aborts before the destructive `DELETE` if the KV snapshot is
/// incomplete** — a transient key-iteration or `kv.get` failure returns
/// an error, leaving the previous projection intact rather than
/// committing a partial snapshot as the full state (CodeRabbit). A
/// per-key *decode* failure is different — that value is genuinely
/// malformed, so it's skipped (warn) and the rest still project.
async fn reconcile(pool: &SqlitePool, kv: &async_nats::jetstream::kv::Store) -> Result<usize> {
    // Gather the whole snapshot first (network reads); only then swap it
    // in one transaction (local writes). Any enumeration/read error aborts
    // here, BEFORE the DELETE, so valid rows are never wiped on a blip.
    let mut keys = kv.keys().await.context("agent_meta reconcile: kv.keys()")?;
    let mut all: Vec<(String, AgentMeta)> = Vec::new();
    while let Some(key) = keys.next().await {
        let pc_id = key.context("agent_meta reconcile: iterate KV keys")?;
        match kv.get(&pc_id).await {
            Ok(Some(b)) => match serde_json::from_slice::<AgentMeta>(&b) {
                Ok(m) => all.push((pc_id, m)),
                Err(e) => {
                    warn!(error = %e, pc_id = %pc_id, "agent_meta reconcile: decode; skipping row")
                }
            },
            Ok(None) => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("agent_meta reconcile: read KV value for {pc_id}"));
            }
        }
    }

    let mut tx = pool
        .begin()
        .await
        .context("begin agent_meta reconcile tx")?;
    sqlx::query("DELETE FROM agent_meta")
        .execute(&mut *tx)
        .await
        .context("clear agent_meta")?;
    let mut rows = 0usize;
    for (pc_id, meta) in &all {
        for e in &meta.entries {
            sqlx::query("INSERT INTO agent_meta (pc_id, key, value) VALUES (?, ?, ?)")
                .bind(pc_id)
                .bind(&e.key)
                .bind(&e.value)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("insert agent_meta row for {pc_id}"))?;
            rows += 1;
        }
    }
    tx.commit().await.context("commit agent_meta reconcile")?;
    Ok(rows)
}

/// Supervised watch loop: keep the table in sync with every KV write,
/// forever. Mirrors the `schedules` projector (`scheduler::run`):
///
///   * `watch_all` has no initial delivery, so [`reconcile`] runs after
///     each (re)attach to pick up writes made while the watch was down.
///   * A watch-stream end or attach failure reopens after
///     [`REOPEN_BACKOFF`] instead of halting the projection until the
///     next backend restart.
///
/// The buffered events drained after `reconcile` re-apply idempotently
/// (`replace_pc` is a full per-PC swap), so the reconcile→drain overlap
/// can't corrupt state.
pub async fn run(pool: SqlitePool, jetstream: jetstream::Context) -> Result<()> {
    let kv = jetstream
        .get_key_value(BUCKET_AGENT_META)
        .await
        .with_context(|| format!("get KV {BUCKET_AGENT_META} for agent_meta watcher"))?;
    loop {
        let mut watcher = match kv.watch_all().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "agent_meta watch_all failed; retrying");
                tokio::time::sleep(REOPEN_BACKOFF).await;
                continue;
            }
        };
        // Catch up on everything written before/while the watch was down.
        match reconcile(&pool, &kv).await {
            Ok(rows) => info!(rows, "agent_meta projector (re)attached + reconciled"),
            Err(e) => warn!(error = %e, "agent_meta reconcile after watch (re)open failed"),
        }
        while let Some(entry) = watcher.next().await {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "agent_meta watch: entry error; continuing");
                    continue;
                }
            };
            match entry.operation {
                Operation::Put => match serde_json::from_slice::<AgentMeta>(&entry.value) {
                    Ok(meta) => {
                        if let Err(e) = replace_pc(&pool, &entry.key, &meta).await {
                            warn!(error = %e, pc_id = %entry.key, "agent_meta watch: replace failed");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, pc_id = %entry.key, "agent_meta watch: decode failed")
                    }
                },
                Operation::Delete | Operation::Purge => {
                    if let Err(e) = delete_pc(&pool, &entry.key).await {
                        warn!(error = %e, pc_id = %entry.key, "agent_meta watch: delete failed");
                    }
                }
            }
        }
        warn!("agent_meta watch ended; reopening");
        tokio::time::sleep(REOPEN_BACKOFF).await;
    }
}

/// Replace one PC's rows from an `AgentMeta` value. Delete-then-insert in
/// one transaction so a reader never sees a half-updated set for the PC.
async fn replace_pc(pool: &SqlitePool, pc_id: &str, meta: &AgentMeta) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM agent_meta WHERE pc_id = ?")
        .bind(pc_id)
        .execute(&mut *tx)
        .await?;
    for e in &meta.entries {
        sqlx::query("INSERT INTO agent_meta (pc_id, key, value) VALUES (?, ?, ?)")
            .bind(pc_id)
            .bind(&e.key)
            .bind(&e.value)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn delete_pc(pool: &SqlitePool, pc_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM agent_meta WHERE pc_id = ?")
        .bind(pc_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::wire::MetaEntry;
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

    fn meta(pairs: &[(&str, &str)]) -> AgentMeta {
        AgentMeta::new(pairs.iter().map(|(k, v)| MetaEntry::new(*k, *v)))
    }

    async fn rows_for(pool: &SqlitePool, pc_id: &str) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT key, value FROM agent_meta WHERE pc_id = ? ORDER BY key",
        )
        .bind(pc_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn replace_pc_upserts_and_deletes_removed_keys() {
        let pool = pool().await;
        replace_pc(&pool, "pc1", &meta(&[("氏名", "山田"), ("所属", "経理")]))
            .await
            .unwrap();
        // ORDER BY key — 所 (U+6240) sorts before 氏 (U+6C0F).
        assert_eq!(
            rows_for(&pool, "pc1").await,
            vec![
                ("所属".to_string(), "経理".to_string()),
                ("氏名".to_string(), "山田".to_string()),
            ]
        );

        // Second write drops 所属, keeps 氏名 (updated), adds メール.
        replace_pc(
            &pool,
            "pc1",
            &meta(&[("氏名", "山田太郎"), ("メール", "y@example.com")]),
        )
        .await
        .unwrap();
        assert_eq!(
            rows_for(&pool, "pc1").await,
            vec![
                ("メール".to_string(), "y@example.com".to_string()),
                ("氏名".to_string(), "山田太郎".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn delete_pc_removes_all_rows() {
        let pool = pool().await;
        replace_pc(&pool, "pc1", &meta(&[("k", "v")]))
            .await
            .unwrap();
        replace_pc(&pool, "pc2", &meta(&[("k", "v")]))
            .await
            .unwrap();
        delete_pc(&pool, "pc1").await.unwrap();
        assert!(rows_for(&pool, "pc1").await.is_empty());
        assert_eq!(rows_for(&pool, "pc2").await.len(), 1, "other PC untouched");
    }
}
