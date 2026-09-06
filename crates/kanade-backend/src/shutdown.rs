//! SQLite retention and orderly backend teardown (#1433).

use std::{future::Future, str::FromStr, time::Duration};

use anyhow::{Context, Result};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use tokio::task::JoinHandle;

// Retained WAL size after reuse, NOT a hard cap: active readers or a large
// transaction can still grow the WAL beyond this until checkpointing succeeds.
const WAL_RETAIN_BYTES: u64 = 16 * 1024 * 1024;
// Keep below deploy-backend.ps1's 30-second service-stop wait.
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_secs(25);

/// Shared writable connection settings, including retention on every connection.
pub(crate) fn sqlite_options(path: &str) -> Result<SqliteConnectOptions> {
    Ok(SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
        .with_context(|| format!("parse sqlite path {path}"))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(30))
        .pragma("journal_size_limit", WAL_RETAIN_BYTES.to_string()))
}

#[derive(Default)]
/// Own pools and top-level tasks independently of cancellable backend work.
pub(crate) struct BackendResources {
    pub writer: Option<SqlitePool>,
    pub reader: Option<SqlitePool>,
    tasks: Vec<JoinHandle<()>>,
}

impl BackendResources {
    /// Spawn a task that must be cancelled and joined before pool closure.
    pub fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        self.track(tokio::spawn(task));
    }

    /// Adopt a task started by a helper such as the periodic cleanup worker.
    pub fn track(&mut self, task: JoinHandle<()>) {
        self.tasks.push(task);
    }

    /// Stop tracked work and await ordered pool closure within the stop budget.
    pub async fn close(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let close = async {
            for task in self.tasks.drain(..) {
                let _ = task.await;
            }
            // Close readers first so the last writable connection can perform
            // SQLite's close-time checkpoint and remove the WAL. Pool::close
            // rejects new acquisitions through ALL clones and waits for checked
            // out connections (including HTTP handlers) to finish.
            if let Some(reader) = &self.reader {
                reader.close().await;
            }
            if let Some(writer) = &self.writer {
                writer.close().await;
            }
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, close).await.is_err() {
            tracing::warn!("SQLite shutdown timed out; WAL recovery will run on next open");
        } else {
            tracing::info!("backend tasks stopped and SQLite pools closed");
        }
    }
}

/// Wait for console Ctrl+C, or SIGTERM when running under a Unix service manager.
pub(crate) async fn console_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn wal_shrinks_on_reuse_and_every_writer_has_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retention.db");
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(sqlite_options(path.to_str().unwrap()).unwrap())
            .await
            .unwrap();
        let mut first = pool.acquire().await.unwrap();
        let mut second = pool.acquire().await.unwrap();
        for connection in [&mut first, &mut second] {
            let limit: i64 = sqlx::query_scalar("PRAGMA journal_size_limit")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            assert_eq!(limit, WAL_RETAIN_BYTES as i64);
        }
        sqlx::query("CREATE TABLE payload (data BLOB)")
            .execute(&mut *first)
            .await
            .unwrap();
        // A single large commit can exceed the retention limit. Its automatic
        // checkpoint completes, then the next write reuses and shrinks the WAL.
        sqlx::query("INSERT INTO payload VALUES (zeroblob(20971520))")
            .execute(&mut *first)
            .await
            .unwrap();
        let wal = dir.path().join("retention.db-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > WAL_RETAIN_BYTES);
        sqlx::query("INSERT INTO payload VALUES (X'01')")
            .execute(&mut *first)
            .await
            .unwrap();
        assert!(std::fs::metadata(&wal).unwrap().len() <= WAL_RETAIN_BYTES);
        drop(first);
        drop(second);
        pool.close().await;
    }

    #[tokio::test]
    async fn shutdown_releases_tasks_waits_for_readers_and_preserves_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shutdown.db");
        let writer = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(sqlite_options(path.to_str().unwrap()).unwrap())
            .await
            .unwrap();
        sqlx::query("CREATE TABLE payload (id INTEGER)")
            .execute(&writer)
            .await
            .unwrap();
        sqlx::query("INSERT INTO payload VALUES (1)")
            .execute(&writer)
            .await
            .unwrap();
        let reader = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path).read_only(true))
            .await
            .unwrap();
        let mut read_tx = reader.begin().await.unwrap();
        let _: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payload")
            .fetch_one(&mut *read_tx)
            .await
            .unwrap();
        let mut resources = BackendResources {
            writer: Some(writer.clone()),
            reader: Some(reader.clone()),
            ..Default::default()
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task_pool = writer.clone();
        resources.spawn(async move {
            let mut tx = task_pool.begin().await.unwrap();
            sqlx::query("INSERT INTO payload VALUES (2)")
                .execute(&mut *tx)
                .await
                .unwrap();
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
            drop(tx);
        });
        ready_rx.await.unwrap();
        let closing = tokio::spawn(async move { resources.close().await });
        tokio::time::timeout(Duration::from_secs(5), reader.close_event())
            .await
            .unwrap();
        assert!(
            !closing.is_finished(),
            "shutdown must wait for the borrowed reader"
        );
        read_tx.rollback().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .unwrap()
            .unwrap();
        assert!(writer.is_closed());
        assert!(reader.is_closed());
        let wal = dir.path().join("shutdown.db-wal");
        assert!(!wal.exists() || std::fs::metadata(&wal).unwrap().len() == 0);
        let reopened = SqlitePoolOptions::new()
            .connect_with(sqlite_options(path.to_str().unwrap()).unwrap())
            .await
            .unwrap();
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM payload ORDER BY id")
            .fetch_all(&reopened)
            .await
            .unwrap();
        assert_eq!(
            ids,
            vec![1],
            "committed data survives; interrupted transaction rolls back"
        );
        reopened.close().await;
    }
}
