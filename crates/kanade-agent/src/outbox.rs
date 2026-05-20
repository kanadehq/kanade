//! v0.24: file-based outbox for ExecResult publishes.
//!
//! When the agent finishes a script (live-NATS Command, replay'd
//! Command, or `runs_on: agent` local tick), it produces an
//! [`ExecResult`] to be delivered to the backend via the `results.<id>`
//! subject. Before v0.24 the agent did a raw `client.publish()` and
//! trusted async-nats's reconnect buffer. That works for seconds-to-
//! minutes outages but loses data on:
//!
//!   * agent crash between script-finish and publish
//!   * broker dead longer than the client's buffer (default 64 MB)
//!   * agent restart while the client buffer holds pending msgs
//!
//! Outbox flow:
//!
//!   1. `enqueue(path, result)` writes `<outbox_dir>/<request_id>.json`
//!      atomically (tmp → rename). Synchronous + fast.
//!   2. A long-running `drain` task scans `outbox_dir` periodically,
//!      reads each file, and calls `js.publish().await.await` (= wait
//!      for the JetStream PubAck). Only on Ack does it delete the
//!      file. Failures leave the file in place and retry next loop.
//!
//! Idempotency: the backend's `results` projector already does
//! `INSERT ... ON CONFLICT(request_id) DO NOTHING`, so re-delivery of
//! the same result on agent restart is harmless.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use kanade_shared::ExecResult;
use kanade_shared::subject;
use tracing::{debug, info, warn};

/// Drain-loop poll interval. Aggressive on purpose so a result
/// published to disk reaches the broker within ~ a second when
/// online. JetStream publish itself blocks on Ack, so an offline
/// broker just makes the next iteration sit on the publish future
/// — no busy loop.
const DRAIN_INTERVAL: Duration = Duration::from_secs(1);

/// Atomically persist a single result to the outbox dir. Returns
/// the full path of the persisted file so the caller can log /
/// debug if needed. Use `request_id` as the file name so the
/// drain task can re-publish in arrival order (`request_id`s are
/// UUIDs so collisions are practically impossible).
pub fn enqueue(outbox_dir: &Path, result: &ExecResult) -> Result<PathBuf> {
    std::fs::create_dir_all(outbox_dir)
        .with_context(|| format!("create outbox dir {outbox_dir:?}"))?;
    let final_path = outbox_dir.join(format!("{}.json", result.request_id));
    let tmp_path = outbox_dir.join(format!("{}.json.tmp", result.request_id));
    let bytes = serde_json::to_vec(result).context("serialise ExecResult")?;
    std::fs::write(&tmp_path, &bytes)
        .with_context(|| format!("write tmp outbox file {tmp_path:?}"))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("rename tmp → {final_path:?}"))?;
    Ok(final_path)
}

/// Spawn the drain task. Lives for the agent process's lifetime;
/// each iteration scans `outbox_dir`, publishes every file via
/// `js.publish().await` (JetStream PubAck waited on), and deletes
/// each that succeeds. Failures (broker down, publish error) just
/// leave the file in place.
pub fn spawn_drain(client: async_nats::Client, outbox_dir: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let js = async_nats::jetstream::new(client);
        // Mirroring events_outbox::spawn_drain (Gemini #73 high fix):
        // retry mkdir on each loop iteration instead of dying forever.
        // The original "die on mkdir failure" was a startup
        // optimization but it makes a recoverable failure (permission
        // / disk-full / parent-not-bootstrapped) permanent for the
        // process — every subsequent enqueue would write to a dir
        // the drain task gave up on.
        loop {
            if let Err(e) = std::fs::create_dir_all(&outbox_dir) {
                warn!(
                    error = %e,
                    dir = %outbox_dir.display(),
                    "outbox: create dir failed; will retry next tick",
                );
                tokio::time::sleep(DRAIN_INTERVAL).await;
                continue;
            }
            drain_once(&js, &outbox_dir).await;
            tokio::time::sleep(DRAIN_INTERVAL).await;
        }
    })
}

async fn drain_once(js: &async_nats::jetstream::Context, outbox_dir: &Path) {
    let entries = match std::fs::read_dir(outbox_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, dir = %outbox_dir.display(), "outbox: read_dir failed");
            return;
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|r| r.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    if files.is_empty() {
        return;
    }
    // Sort so older results publish first (UUIDs lexicographically
    // sort roughly by time for v4 with random + system clock — not
    // perfect, but file mtime is more reliable).
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());

    // Mirroring events_outbox::drain_once (Gemini #73 high fix):
    // don't `return` on a single file's failure — continue to
    // subsequent files. Broker-down sweeps now log each (debug,
    // low noise) before sleeping; the upside is that a single
    // problematic file no longer pins the entire outbox behind it.
    for path in files {
        if let Err(e) = publish_one(js, &path).await {
            debug!(
                error = %e,
                path = %path.display(),
                "outbox: publish failed for this file; will retry next tick, continuing with others",
            );
        }
    }
}

async fn publish_one(js: &async_nats::jetstream::Context, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let result: ExecResult =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {path:?}"))?;
    let subj = subject::results(&result.request_id);
    let ack_future = js
        .publish(subj.clone(), bytes.clone().into())
        .await
        .with_context(|| format!("publish {subj}"))?;
    // ack_future resolves with the PubAck when the broker has the
    // message durably; awaiting blocks while the broker is
    // unreachable. That blocking IS the point — we don't want to
    // delete the outbox file before we know the broker has it.
    let _ack = ack_future.await.with_context(|| format!("ack {subj}"))?;
    std::fs::remove_file(path).with_context(|| format!("remove {path:?}"))?;
    info!(
        request_id = %result.request_id,
        subject = %subj,
        "outbox: result delivered + file removed",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn sample(rid: &str) -> ExecResult {
        ExecResult {
            result_id: uuid::Uuid::new_v4().to_string(),
            request_id: rid.into(),
            exec_id: None,
            pc_id: "minipc".into(),
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
            started_at: Utc.with_ymd_and_hms(2026, 5, 19, 0, 0, 0).unwrap(),
            finished_at: Utc.with_ymd_and_hms(2026, 5, 19, 0, 0, 1).unwrap(),
            manifest_id: Some("inventory-hw".into()),
        }
    }

    #[test]
    fn enqueue_creates_file_with_request_id_name() {
        let dir = tempfile::tempdir().unwrap();
        let r = sample("req-abc");
        let path = enqueue(dir.path(), &r).unwrap();
        assert_eq!(path.file_name().unwrap(), "req-abc.json");
        let bytes = std::fs::read(&path).unwrap();
        let back: ExecResult = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.request_id, r.request_id);
        assert_eq!(back.exit_code, 0);
    }

    #[test]
    fn enqueue_atomic_overwrite_preserves_old_on_failure() {
        // We can't easily induce a write failure in unit test land,
        // but the tmp + rename approach means a successful enqueue
        // is always a complete file — partial writes don't reach the
        // final name. Cover the happy "overwrite" path.
        let dir = tempfile::tempdir().unwrap();
        let r1 = sample("req-x");
        let r2 = ExecResult {
            exit_code: 7,
            ..sample("req-x")
        };
        enqueue(dir.path(), &r1).unwrap();
        let path = enqueue(dir.path(), &r2).unwrap();
        let back: ExecResult = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.exit_code, 7);
    }
}
