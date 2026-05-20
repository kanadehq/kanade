//! v0.30 / PR α: file-based outbox for `EventStarted` publishes.
//!
//! Mirrors the [`crate::outbox`] design for ExecResult — atomic
//! tmp-then-rename enqueue + a background drain task that publishes
//! via JetStream and only deletes the file on PubAck. Kept as a
//! separate module (instead of generalising `outbox` to take any
//! `Serialize` payload + subject) so the existing v0.24 ExecResult
//! outbox files on disk keep working unchanged across the upgrade —
//! the alternative would have required a one-shot file-format
//! migration on startup.
//!
//! Why an outbox here at all: agents that go offline mid-Command
//! (or fire a `runs_on: agent` schedule while the broker is down)
//! still need their lifecycle events to reach the backend on
//! reconnect so the Activity Running tab catches up. Without
//! persistence, a process-restart between "spawn" and "publish"
//! would lose the started event, and the matching ExecResult would
//! arrive at the backend with no preceding running row to update.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use kanade_shared::subject;
use kanade_shared::wire::EventStarted;
use tracing::{debug, info, warn};

/// Same 1 s cadence as the ExecResult outbox — a started event
/// published while online reaches the broker within ~ a second.
/// Offline tick just sits on the publish future until reconnect.
const DRAIN_INTERVAL: Duration = Duration::from_secs(1);

/// Atomically persist one `EventStarted` to `events_outbox_dir`.
/// Filename is `<exec_id>__<pc_id>.json` so a single agent firing
/// the same exec twice (shouldn't happen — DedupCache prevents it)
/// would overwrite-not-duplicate, and the drain ordering is by
/// mtime (close enough to start-order).
pub fn enqueue(events_outbox_dir: &Path, event: &EventStarted) -> Result<PathBuf> {
    std::fs::create_dir_all(events_outbox_dir)
        .with_context(|| format!("create events outbox dir {events_outbox_dir:?}"))?;
    let stem = format!("{}__{}", event.exec_id, event.pc_id);
    let final_path = events_outbox_dir.join(format!("{stem}.json"));
    let tmp_path = events_outbox_dir.join(format!("{stem}.json.tmp"));
    let bytes = serde_json::to_vec(event).context("serialise EventStarted")?;
    std::fs::write(&tmp_path, &bytes)
        .with_context(|| format!("write tmp events outbox file {tmp_path:?}"))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("rename tmp → {final_path:?}"))?;
    Ok(final_path)
}

/// Spawn the drain task. Long-running; each iteration scans
/// `events_outbox_dir`, publishes pending events via
/// `js.publish().await.await` (PubAck-waited), deletes on success.
/// Mirrors `outbox::spawn_drain`.
pub fn spawn_drain(
    client: async_nats::Client,
    events_outbox_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = std::fs::create_dir_all(&events_outbox_dir) {
            warn!(
                error = %e,
                dir = %events_outbox_dir.display(),
                "events_outbox: create dir failed; drain task exiting",
            );
            return;
        }
        let js = async_nats::jetstream::new(client);
        loop {
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
    // mtime-based ordering so the backend projector sees `started`
    // before `finished` (the ExecResult outbox drains in parallel).
    // Not strict — both outboxes drain independently — but ordering
    // is usually preserved because `enqueue` for the started event
    // happens before script spawn, and for ExecResult after the
    // script returns.
    //
    // Gemini #72 medium fix: `sort_by_cached_key` instead of
    // `sort_by_key` so each file's mtime is read once (not O(log n)
    // times in the comparator). Negligible at low file counts, but
    // shows up after a long broker outage with hundreds of pending
    // events.
    files.sort_by_cached_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());

    for path in files {
        if let Err(e) = publish_one(js, &path).await {
            debug!(
                error = %e,
                path = %path.display(),
                "events_outbox: publish failed; will retry next tick",
            );
            return;
        }
    }
}

async fn publish_one(js: &async_nats::jetstream::Context, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    // Gemini #72 high fix: a corrupted JSON file would otherwise
    // bubble Err up to drain_once, which `return`s early on any
    // error — pinning the whole outbox behind one bad file. Treat
    // parse failure as "this file is broken, drop it and continue"
    // instead. The event is lost but the agent doesn't get stuck.
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
    let _ack = ack_future.await.with_context(|| format!("ack {subj}"))?;
    std::fs::remove_file(path).with_context(|| format!("remove {path:?}"))?;
    info!(
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

    fn sample(exec_id: &str, pc_id: &str) -> EventStarted {
        EventStarted {
            exec_id: exec_id.into(),
            pc_id: pc_id.into(),
            started_at: Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap(),
            manifest_id: "inv-hw".into(),
            version: "1.0.0".into(),
        }
    }

    #[test]
    fn enqueue_creates_file_with_combined_name() {
        let dir = tempfile::tempdir().unwrap();
        let e = sample("exec-1", "minipc");
        let path = enqueue(dir.path(), &e).unwrap();
        assert_eq!(path.file_name().unwrap(), "exec-1__minipc.json");
        let bytes = std::fs::read(&path).unwrap();
        let back: EventStarted = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.exec_id, e.exec_id);
        assert_eq!(back.pc_id, e.pc_id);
    }

    #[test]
    fn enqueue_overwrite_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let e1 = sample("exec-x", "pc-1");
        let e2 = EventStarted {
            manifest_id: "different".into(),
            ..sample("exec-x", "pc-1")
        };
        enqueue(dir.path(), &e1).unwrap();
        let path = enqueue(dir.path(), &e2).unwrap();
        let back: EventStarted = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.manifest_id, "different");
    }
}
