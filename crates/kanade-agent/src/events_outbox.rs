//! v0.30 / PR α' — file-based outbox for `EventStarted` publishes.
//!
//! Mirrors `crate::outbox`'s ExecResult design: atomic tmp-then-
//! rename enqueue + a background drain task that publishes via
//! JetStream PubAck, deleting the file only on ack. Separate
//! directory (`events-outbox/`) so existing v0.24 ExecResult outbox
//! files keep working unchanged across the upgrade.
//!
//! Why a parallel outbox instead of one generic envelope: lets the
//! existing on-disk format keep its meaning literally (= `<rid>.json`
//! IS an ExecResult, no envelope wrapping needed), at the cost of
//! ~80 lines of duplication. Avoids a one-shot file-format
//! migration on startup.
//!
//! Offline-safety: an agent that goes offline mid-Command still has
//! its lifecycle events reach the backend on reconnect. Without
//! persistence, a process restart between "spawn" and "publish"
//! would lose the started event entirely, and the matching
//! ExecResult arriving later would create the execution_results
//! row with `started_at` from the result side and no version/manifest
//! propagation from the start side — survivable but incomplete.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use kanade_shared::subject;
use kanade_shared::wire::EventStarted;
use tracing::{debug, info, warn};

const DRAIN_INTERVAL: Duration = Duration::from_secs(1);

/// Hard upper bound on how long we'll wait for a single publish's
/// PubAck before giving up on that file for this drain iteration.
/// Matches `outbox::ACK_TIMEOUT`; see that constant's doc for the
/// rationale (#139).
const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Atomically persist one `EventStarted` to `events_outbox_dir`.
/// Filename is `<result_id>.json` — guaranteed unique per (exec,
/// pc) run because the agent mints `result_id` once per
/// handle_command invocation.
pub fn enqueue(events_outbox_dir: &Path, event: &EventStarted) -> Result<PathBuf> {
    std::fs::create_dir_all(events_outbox_dir)
        .with_context(|| format!("create events outbox dir {events_outbox_dir:?}"))?;
    let final_path = events_outbox_dir.join(format!("{}.json", event.result_id));
    let tmp_path = events_outbox_dir.join(format!("{}.json.tmp", event.result_id));
    let bytes = serde_json::to_vec(event).context("serialise EventStarted")?;
    std::fs::write(&tmp_path, &bytes)
        .with_context(|| format!("write tmp events outbox file {tmp_path:?}"))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("rename tmp → {final_path:?}"))?;
    Ok(final_path)
}

/// Long-running drain task. Each iteration scans the directory,
/// publishes pending events via `js.publish().await.await`
/// (PubAck-waited), deletes on success. Mirrors
/// `outbox::spawn_drain`.
pub fn spawn_drain(
    client: async_nats::Client,
    events_outbox_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let js = async_nats::jetstream::new(client);
        // Gemini #73 high fix: retry mkdir on each loop iteration
        // instead of dying forever. Permission / disk-full / parent-
        // missing failures are operationally recoverable, and dying
        // here would silently swallow every subsequent enqueue.
        loop {
            if let Err(e) = std::fs::create_dir_all(&events_outbox_dir) {
                warn!(
                    error = %e,
                    dir = %events_outbox_dir.display(),
                    "events_outbox: create dir failed; will retry next tick",
                );
                tokio::time::sleep(DRAIN_INTERVAL).await;
                continue;
            }
            drain_once(&js, &events_outbox_dir).await;
            tokio::time::sleep(DRAIN_INTERVAL).await;
        }
    })
}

async fn drain_once(js: &async_nats::jetstream::Context, events_outbox_dir: &Path) {
    let entries = match std::fs::read_dir(events_outbox_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                error = %e,
                dir = %events_outbox_dir.display(),
                "events_outbox: read_dir failed",
            );
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
    // mtime-based ordering — `sort_by_cached_key` reads each file's
    // metadata once instead of in every comparator step. Negligible
    // at low file counts but matters when a long broker outage left
    // hundreds of pending events.
    files.sort_by_cached_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());

    // Gemini #73 high fix: don't `return` on a single file's
    // failure — continue to subsequent files. The original
    // "stop on first error" was meant as backpressure for the
    // common broker-down case (every publish_one would fail).
    // Trade: a broker-down sweep now tries every file and logs
    // each (debug level, low noise) before sleeping. Upside: a
    // single problematic file (transient NATS error specific to
    // its payload, ack timeout, etc.) no longer pins the entire
    // outbox behind it. The remaining unpublished files stay on
    // disk for the next tick to retry.
    for path in files {
        if let Err(e) = publish_one(js, &path).await {
            debug!(
                error = %e,
                path = %path.display(),
                "events_outbox: publish failed for this file; will retry next tick, continuing with others",
            );
        }
    }
}

async fn publish_one(js: &async_nats::jetstream::Context, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    // Treat parse failure as "drop the corrupted file and continue"
    // rather than propagating Err — drain_once bails on any error
    // from publish_one, so a single corrupted file would otherwise
    // wedge the whole outbox.
    let event: EventStarted = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "events_outbox: corrupted file — removing so drain can proceed",
            );
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
    };
    let subj = subject::events_started(&event.exec_id, &event.pc_id);
    let ack_future = js
        .publish(subj.clone(), bytes.clone().into())
        .await
        .with_context(|| format!("publish {subj}"))?;
    // Bounded ack wait (#139): a wedged broker or a stalled
    // upstream stream must not pin the whole drain loop behind one
    // file. On timeout the file stays on disk for the next drain
    // iteration to retry.
    let _ack = tokio::time::timeout(ACK_TIMEOUT, ack_future)
        .await
        .with_context(|| format!("ack timeout {subj} after {}s", ACK_TIMEOUT.as_secs()))?
        .with_context(|| format!("ack {subj}"))?;
    std::fs::remove_file(path).with_context(|| format!("remove {path:?}"))?;
    info!(
        result_id = %event.result_id,
        exec_id = %event.exec_id,
        pc_id = %event.pc_id,
        subject = %subj,
        "events_outbox: started event delivered + file removed",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn sample(result_id: &str, exec_id: &str, pc_id: &str) -> EventStarted {
        EventStarted {
            result_id: result_id.into(),
            request_id: "req-1".into(),
            exec_id: exec_id.into(),
            pc_id: pc_id.into(),
            started_at: Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap(),
            manifest_id: "inv-hw".into(),
            version: "1.0.0".into(),
        }
    }

    #[test]
    fn enqueue_creates_file_named_by_result_id() {
        let dir = tempfile::tempdir().unwrap();
        let e = sample("res-1", "exec-1", "pc-01");
        let path = enqueue(dir.path(), &e).unwrap();
        assert_eq!(path.file_name().unwrap(), "res-1.json");
        let back: EventStarted = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.result_id, "res-1");
        assert_eq!(back.exec_id, "exec-1");
    }

    #[test]
    fn enqueue_atomic_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let e1 = sample("res-x", "exec-x", "pc-1");
        let e2 = EventStarted {
            manifest_id: "different".into(),
            ..sample("res-x", "exec-x", "pc-1")
        };
        enqueue(dir.path(), &e1).unwrap();
        let path = enqueue(dir.path(), &e2).unwrap();
        let back: EventStarted = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.manifest_id, "different");
    }
}
