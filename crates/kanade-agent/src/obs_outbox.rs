//! Issue #246 — file-based outbox for `ObsEvent` publishes on
//! `obs.<pc_id>`.
//!
//! Mirrors `crate::events_outbox`'s design (atomic tmp-then-rename,
//! background drain task that publishes via JetStream PubAck, file
//! deleted on ack) but for the per-PC observability timeline stream.
//! Separate directory (`obs-outbox/`) so existing events_outbox and
//! ExecResult outbox files keep working unchanged across the upgrade.
//!
//! Filename strategy: each enqueued event gets a fresh UUID since
//! `ObsEvent` doesn't carry a `result_id` (events are bulk emissions
//! from a single script run — 50+ per poll is normal). Backend
//! dedup happens server-side via the UNIQUE
//! `(pc_id, source, event_record_id)` constraint on the
//! `obs_events` table, so a retry that re-publishes the same event
//! is harmless.
//!
//! Offline-safety: an agent that goes offline mid-poll still has
//! the parsed events reach the backend on reconnect. Without
//! persistence, a process restart between "parse stdout" and
//! "publish" would lose every event the script emitted — survivable
//! only because the script's watermark file should still hold the
//! "last reported" position, so the next run would re-emit. But
//! that depends on the script implementing the watermark correctly
//! AND being run by the local scheduler within the JetStream
//! retention window; the outbox makes the "I just emitted these"
//! commitment durable so neither contract has to be perfect.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use kanade_shared::subject;
use kanade_shared::wire::ObsEvent;
use tracing::{debug, warn};
use uuid::Uuid;

const DRAIN_INTERVAL: Duration = Duration::from_secs(1);

/// Hard upper bound on how long we'll wait for a single publish's
/// PubAck before giving up on that file for this drain iteration.
/// Matches `outbox::ACK_TIMEOUT`; see that constant's doc for the
/// rationale (#139).
const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// The agent's standard obs-outbox directory. One source of truth for the
/// callers that don't already receive the path from `main` (the KLP
/// handlers, which are constructed long before it is computed there).
pub fn default_dir() -> PathBuf {
    kanade_shared::default_paths::data_dir().join("obs-outbox")
}

/// Idempotently make sure the outbox directory exists. Cheap on
/// the steady state (one stat call) but on a long-tail-busy agent
/// the per-enqueue `create_dir_all` adds up — Gemini #249 medium.
/// Hoist this to the caller (e.g. `forward_obs_events` calls once
/// before the loop, drain task calls once per tick) so the
/// per-event `enqueue` skips the syscall entirely on the hot path.
pub fn ensure_outbox_dir(obs_outbox_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(obs_outbox_dir)
        .with_context(|| format!("create obs outbox dir {obs_outbox_dir:?}"))
}

/// Atomically persist one `ObsEvent` to `obs_outbox_dir`. Filename
/// is `<uuid>.json` — the UUID exists only for filesystem
/// uniqueness; the event's own dedup key is the
/// `(pc_id, source, event_record_id)` triplet that the backend's
/// UNIQUE constraint enforces.
///
/// PRE-CONDITION: `obs_outbox_dir` exists. Call
/// [`ensure_outbox_dir`] once before a batch of enqueues to make
/// this side-effect-free.
pub fn enqueue(obs_outbox_dir: &Path, event: &ObsEvent) -> Result<PathBuf> {
    let file_id = Uuid::new_v4().simple().to_string();
    let final_path = obs_outbox_dir.join(format!("{file_id}.json"));
    let tmp_path = obs_outbox_dir.join(format!("{file_id}.json.tmp"));
    let bytes = serde_json::to_vec(event).context("serialise ObsEvent")?;
    std::fs::write(&tmp_path, &bytes)
        .with_context(|| format!("write tmp obs outbox file {tmp_path:?}"))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("rename tmp → {final_path:?}"))?;
    Ok(final_path)
}

/// Long-running drain task. Each iteration scans the directory,
/// publishes pending events via `js.publish().await.await`
/// (PubAck-waited), deletes on success. Same shape as
/// `events_outbox::spawn_drain` — the broker-down / disk-full /
/// permission-error recovery posture is identical.
pub fn spawn_drain(
    client: async_nats::Client,
    obs_outbox_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let js = async_nats::jetstream::new(client);

        // Per-file retry state, owned by the drain task so it survives across
        // ticks. A ledger scoped to one `drain_once` call would reset every
        // second and back off nothing.
        let mut ledger = crate::outbox_retry::RetryLedger::new();
        loop {
            if let Err(e) = std::fs::create_dir_all(&obs_outbox_dir) {
                warn!(
                    error = %e,
                    dir = %obs_outbox_dir.display(),
                    "obs_outbox: create dir failed; will retry next tick",
                );
                tokio::time::sleep(DRAIN_INTERVAL).await;
                continue;
            }
            drain_once(&js, &obs_outbox_dir, &mut ledger).await;
            tokio::time::sleep(DRAIN_INTERVAL).await;
        }
    })
}

async fn drain_once(
    js: &async_nats::jetstream::Context,
    obs_outbox_dir: &Path,
    ledger: &mut crate::outbox_retry::RetryLedger,
) {
    let entries = match std::fs::read_dir(obs_outbox_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                error = %e,
                dir = %obs_outbox_dir.display(),
                "obs_outbox: read_dir failed",
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
    files.sort_by_cached_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());

    // Forget files that have left the directory, so the ledger tracks the
    // currently-stuck set rather than everything ever seen (#1319).
    ledger.retain_present(&files);
    for path in files {
        // Read per file, not once per sweep. Each `publish_one` can block up
        // to `ACK_TIMEOUT`, so a sweep over a long queue outlasts the drain
        // interval by minutes — anchoring every decision to the instant the
        // sweep began would shorten the real backoff and skip files that
        // became due while it ran.
        let now = std::time::Instant::now();
        // Checked BEFORE the read, not after: `publish_one` opens with
        // `std::fs::read` of the whole file, and re-reading the queue every
        // second is what pinned a core for two days. Skipping here is the
        // entire saving.
        if !ledger.is_due(&path, now) {
            continue;
        }
        match publish_one(js, &path).await {
            Ok(()) => ledger.record_success(&path),
            Err(e) => {
                let outcome = ledger.record_failure(&path, now);
                if ledger.should_warn(&path) {
                    warn!(
                        error = %e,
                        path = %path.display(),
                        failures = ledger.failures(&path),
                        "obs_outbox: publish keeps failing for this file",
                    );
                } else {
                    debug!(
                        error = %e,
                        path = %path.display(),
                        "obs_outbox: publish failed; backing off, continuing with others",
                    );
                }
                if outcome == crate::outbox_retry::AfterFailure::Quarantine
                    && crate::outbox_retry::quarantine(&path, "obs_outbox")
                {
                    // Only once the file has actually left the directory.
                    // Clearing it for a file still sitting there would
                    // restart it at zero failures with no backoff — the 1 Hz
                    // re-read this change removes, back again.
                    ledger.record_success(&path);
                }
            }
        }
    }
}

async fn publish_one(js: &async_nats::jetstream::Context, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let event: ObsEvent = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "obs_outbox: corrupted file — removing so drain can proceed",
            );
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
    };
    let subj = subject::obs(&event.pc_id);
    // Gemini #249 medium: pass `bytes` by move (`bytes.into()`)
    // instead of `bytes.clone().into()`. `bytes` is unused after
    // this call, so the clone was an unnecessary heap allocation
    // per published event — significant at the ~50/day/PC × N PCs
    // scale.
    let ack_future = js
        .publish(subj.clone(), bytes.into())
        .await
        .with_context(|| format!("publish {subj}"))?;
    let _ack = tokio::time::timeout(ACK_TIMEOUT, ack_future)
        .await
        .with_context(|| format!("ack timeout {subj} after {}s", ACK_TIMEOUT.as_secs()))?
        .with_context(|| format!("ack {subj}"))?;
    std::fs::remove_file(path).with_context(|| format!("remove {path:?}"))?;
    debug!(
        pc_id = %event.pc_id,
        kind = %event.kind,
        source = %event.source,
        subject = %subj,
        "obs_outbox: event delivered + file removed",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    fn sample(pc_id: &str, event_record_id: &str) -> ObsEvent {
        ObsEvent {
            pc_id: pc_id.into(),
            at: Utc.with_ymd_and_hms(2026, 5, 28, 10, 41, 0).unwrap(),
            kind: "logon".into(),
            source: "winlog:Security".into(),
            event_record_id: Some(event_record_id.into()),
            payload: json!({ "user": "yukimemi" }),
        }
    }

    #[test]
    fn enqueue_creates_uuid_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let e = sample("pc-01", "1");
        let path = enqueue(dir.path(), &e).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        // 32 hex chars + `.json` suffix
        assert!(name.ends_with(".json"), "expected .json suffix on {name}");
        assert_eq!(
            name.len(),
            32 + ".json".len(),
            "UUID simple form is 32 chars; got {name}"
        );
        let back: ObsEvent = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.pc_id, "pc-01");
        assert_eq!(back.event_record_id.as_deref(), Some("1"));
    }

    #[test]
    fn enqueue_multiple_events_creates_distinct_files() {
        // ObsEvent has no result_id-style natural key, so the
        // outbox uses fresh UUIDs per enqueue. Two enqueues of
        // semantically-identical events still produce two files
        // (server-side UNIQUE constraint collapses them at insert
        // time).
        let dir = tempfile::tempdir().unwrap();
        let e = sample("pc-01", "1");
        let p1 = enqueue(dir.path(), &e).unwrap();
        let p2 = enqueue(dir.path(), &e).unwrap();
        assert_ne!(p1, p2, "two enqueues should land on distinct paths");
    }
}
