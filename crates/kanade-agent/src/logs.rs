//! `logs.fetch.<pc_id>` request/reply handler.
//!
//! On-demand only — the operator (or backend, on the Web UI's
//! behalf) sends a [`LogsRequest`] with the desired line count and
//! the agent replies with the tail of its most-recent rolling log
//! file as UTF-8 bytes. We never push logs upstream on our own.
//!
//! Reply size is bounded by the requested `tail_lines` plus a hard
//! ceiling of 800 KB (well under the default 1 MB NATS payload limit
//! — leaves headroom for the surrounding headers / wire overhead).
//! Operators who want a whole file or multiple-day spans will get a
//! follow-up Object-Store-backed path; the inline `tail_lines` form
//! covers the "drop into one box and see what just went wrong" case.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::LogsRequest;
use tracing::{info, warn};

/// Soft cap on each reply. Keeps any single oversized line set from
/// blowing past NATS' default 1 MB max payload.
const REPLY_BYTE_CAP: usize = 800 * 1024;

/// Subscribe to `logs.fetch.<pc_id>` and serve every incoming
/// request. Each request runs synchronously on this task — log
/// fetches are infrequent enough that fan-out spawning would just
/// add complexity.
pub async fn serve(client: async_nats::Client, pc_id: String, log_path: PathBuf) {
    let subject = subject::logs_fetch(&pc_id);
    let mut sub = match client.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, subject = %subject, "subscribe logs.fetch failed");
            return;
        }
    };
    info!(subject = %subject, log_path = %log_path.display(), "logs.fetch handler ready");

    while let Some(msg) = sub.next().await {
        let Some(reply) = msg.reply.clone() else {
            warn!("logs.fetch without reply subject — operator must use request/reply");
            continue;
        };
        let req: LogsRequest = serde_json::from_slice(&msg.payload).unwrap_or_default();
        let body = match read_tail(&log_path, req.tail_lines as usize).await {
            Ok(b) => b,
            Err(e) => {
                let err_msg = format!("logs.fetch: {e:#}");
                warn!(error = %e, "logs.fetch handler errored");
                err_msg.into_bytes()
            }
        };
        if let Err(e) = client.publish(reply, body.into()).await {
            warn!(error = %e, "publish logs.fetch reply");
        }
    }
}

/// Read the last `n` lines of the most-recent rolling log file at
/// `path`. tracing-appender produces files like
/// `<dir>/agent.YYYY-MM-DD.log`; `path` from agent.toml is the
/// *template* (no date suffix yet), so we glob the directory for
/// the matching `<stem>.*.<ext>` pattern and pick the alphabetically
/// last match — daily rotation means alphabetical = chronological.
async fn read_tail(path: &Path, n: usize) -> Result<Vec<u8>> {
    let active = locate_active_file(path).await?;
    let bytes = tokio::fs::read(&active)
        .await
        .with_context(|| format!("read {active:?}"))?;

    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }

    // Soft cap — keep the reply under NATS' default payload ceiling.
    // Trim from the START so the most recent lines stay visible.
    let bytes = out.into_bytes();
    if bytes.len() <= REPLY_BYTE_CAP {
        return Ok(bytes);
    }
    let cut = bytes.len() - REPLY_BYTE_CAP;
    // Don't slice mid-UTF-8: walk forward to the next char boundary.
    let mut start = cut;
    while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    Ok(bytes[start..].to_vec())
}

/// Find the active rolling log file. tracing-appender's daily
/// rotation produces `<stem>.YYYY-MM-DD.<ext>` files in the same
/// directory; we pick the alphabetically last one matching the
/// template. Falls back to the bare template path if no rotated
/// files exist yet (e.g. brand-new agent on first day).
///
/// Exposed `pub(crate)` so the KLP `system.log_tail` handler can
/// resolve the same actual file the NATS `logs.fetch` path uses
/// (the bare template stored in `cfg.log.path` is never written
/// to on disk by the appender's `Rotation::DAILY` config).
pub(crate) async fn locate_active_file(template: &Path) -> Result<PathBuf> {
    let dir = template
        .parent()
        .with_context(|| format!("log template '{}' has no parent dir", template.display()))?;
    let stem = template
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agent");
    let ext = template
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("log");
    let prefix = format!("{stem}.");
    let suffix = format!(".{ext}");

    let mut rd = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("read_dir {dir:?}"))?;
    let mut best: Option<PathBuf> = None;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(&suffix) {
            continue;
        }
        let p = entry.path();
        match &best {
            Some(curr) if name <= curr.file_name().unwrap().to_str().unwrap() => {}
            _ => best = Some(p),
        }
    }
    Ok(best.unwrap_or_else(|| template.to_path_buf()))
}
