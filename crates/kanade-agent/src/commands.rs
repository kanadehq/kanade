use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use anyhow::Result;
use async_nats::jetstream::kv::Store;
use futures::StreamExt;
use kanade_shared::ExecResult;
use kanade_shared::default_paths;
use kanade_shared::kv::{BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_REVOKED};
use kanade_shared::wire::Command;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::outbox;
use crate::process::{ExecOutcome, run_command_with_kill};

/// FIFO-bounded set of recently-seen `request_id`s. Shared between
/// the core-sub `command_loop` and the JetStream-replay
/// `command_replay::run`. Either path may receive a given Command
/// first (live publish via core sub for online agents; replay on
/// reconnect for offline agents); the second arrival is dropped via
/// [`Self::insert`] returning `false`.
pub struct DedupCache {
    seen: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl DedupCache {
    pub fn new(cap: usize) -> Self {
        Self {
            seen: HashSet::with_capacity(cap),
            order: VecDeque::with_capacity(cap),
            cap,
        }
    }
    /// Returns `true` when `id` is newly inserted, `false` when it
    /// was already present (= duplicate, caller should drop).
    pub fn insert(&mut self, id: String) -> bool {
        if self.seen.contains(&id) {
            return false;
        }
        self.seen.insert(id.clone());
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

pub fn shared_dedup_cache() -> Arc<Mutex<DedupCache>> {
    // 4 KB of RAM gets us ~ 128 request_ids; 1024 is generous.
    Arc::new(Mutex::new(DedupCache::new(1024)))
}

pub async fn command_loop(
    client: async_nats::Client,
    pc_id: String,
    dedup: Arc<Mutex<DedupCache>>,
    mut sub: async_nats::Subscriber,
) {
    let jetstream = async_nats::jetstream::new(client.clone());
    let script_current = jetstream.get_key_value(BUCKET_SCRIPT_CURRENT).await.ok();
    let script_status = jetstream.get_key_value(BUCKET_SCRIPT_STATUS).await.ok();
    if script_current.is_none() {
        warn!(
            bucket = BUCKET_SCRIPT_CURRENT,
            "KV bucket missing — version-pinning skipped (run `kanade jetstream setup`)"
        );
    }
    if script_status.is_none() {
        warn!(
            bucket = BUCKET_SCRIPT_STATUS,
            "KV bucket missing — revoke check skipped (run `kanade jetstream setup`)"
        );
    }

    while let Some(msg) = sub.next().await {
        let cmd: Command = match serde_json::from_slice(&msg.payload) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, subject = %msg.subject, "deserialize command");
                continue;
            }
        };
        // Shared with command_replay: if the JetStream replay path
        // already ran this Command on an earlier reconnect (rare but
        // possible), drop the live duplicate here.
        if !dedup.lock().await.insert(cmd.request_id.clone()) {
            debug!(
                request_id = %cmd.request_id,
                "core-sub dedup: already seen via replay or earlier delivery",
            );
            continue;
        }
        let client = client.clone();
        let pc_id = pc_id.clone();
        let cur = script_current.clone();
        let sta = script_status.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_command(client, pc_id, cmd, cur, sta).await {
                error!(error = %e, "command handler failed");
            }
        });
    }
}

pub async fn handle_command(
    client: async_nats::Client,
    pc_id: String,
    cmd: Command,
    script_current: Option<Store>,
    script_status: Option<Store>,
) -> Result<()> {
    // Spec §2.6 Layer 2: version-pinning + revoke check
    if let Some(cur) = &script_current
        && let Ok(Some(entry)) = cur.get(&cmd.id).await
    {
        let expected = String::from_utf8_lossy(&entry).to_string();
        if expected != cmd.version {
            warn!(
                cmd_id = %cmd.id,
                expected = %expected,
                got = %cmd.version,
                request_id = %cmd.request_id,
                "skip stale command (version mismatch)",
            );
            return Ok(());
        }
    }
    if let Some(sta) = &script_status
        && let Ok(Some(entry)) = sta.get(&cmd.id).await
    {
        let status = String::from_utf8_lossy(&entry).to_string();
        if status == SCRIPT_STATUS_REVOKED {
            warn!(
                cmd_id = %cmd.id,
                request_id = %cmd.request_id,
                "skip revoked command",
            );
            return Ok(());
        }
    }

    // v0.22: deadline_at gates "missed deadline" commands. When the
    // scheduler stamps a deadline and the agent receives the Command
    // after that absolute time (offline reconnect, broker queueing,
    // etc.), publish a synthetic skipped-result so the operator sees
    // the outcome on the Results page rather than silence.
    let now = chrono::Utc::now();
    if let Some(deadline) = cmd.deadline_at {
        if should_skip_for_deadline(deadline, now) {
            warn!(
                cmd_id = %cmd.id,
                request_id = %cmd.request_id,
                %deadline,
                %now,
                "skip: starting deadline expired",
            );
            return publish_skipped(&client, &pc_id, &cmd, deadline, now).await;
        }
    }

    info!(
        cmd_id = %cmd.id,
        request_id = %cmd.request_id,
        version = %cmd.version,
        job_id = ?cmd.job_id,
        "executing command",
    );
    let started_at = chrono::Utc::now();
    let outcome = run_command_with_kill(&client, &cmd).await?;
    let finished_at = chrono::Utc::now();

    let (exit_code, stdout, stderr, status_note) = match outcome {
        ExecOutcome::Completed {
            exit_code,
            stdout,
            stderr,
        } => (exit_code, stdout, stderr, None),
        ExecOutcome::Killed { stdout, stderr } => {
            let jid = cmd.job_id.as_deref().unwrap_or("?");
            (
                -1,
                stdout,
                stderr,
                Some(format!("killed by remote signal (kill.{jid})")),
            )
        }
        ExecOutcome::Timeout { stdout, stderr } => (
            -1,
            stdout,
            stderr,
            Some(format!("timeout after {}s", cmd.timeout_secs)),
        ),
    };
    let stderr = match status_note {
        Some(note) if stderr.is_empty() => note,
        Some(note) => format!("{stderr}\n{note}"),
        None => stderr,
    };

    let result = ExecResult {
        request_id: cmd.request_id.clone(),
        pc_id: pc_id.clone(),
        exit_code,
        stdout,
        stderr,
        started_at,
        finished_at,
        // Forward `Command.id` (the manifest's id, e.g. "inventory-hw"),
        // NOT `Command.job_id` (a per-deploy UUID). The backend's
        // results projector uses this to look up the manifest's
        // `inventory:` hint and upsert `inventory_facts` rows.
        manifest_id: Some(cmd.id.clone()),
    };
    let outbox_dir = default_paths::data_dir().join("outbox");
    let path = outbox::enqueue(&outbox_dir, &result)?;
    info!(
        request_id = %cmd.request_id,
        exit_code,
        outbox = %path.display(),
        "result enqueued to outbox (drain task delivers via JetStream)",
    );
    // `client` is now unused on the happy path; suppress the warning
    // — we keep it in the signature so future hooks (audit, kill
    // ack, etc.) have it available.
    let _ = client;
    Ok(())
}

/// Pure deadline check — boundary policy: `now > deadline` skips,
/// `now == deadline` still runs (deadline is the inclusive last
/// instant to start). Kept as a free function so the
/// `should_skip_for_deadline_*` unit tests below can pin the
/// boundary without spinning up tokio / NATS.
fn should_skip_for_deadline(
    deadline: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    now > deadline
}

/// Synthesise an ExecResult that mirrors a real run but flags
/// "didn't actually run because we were too late". Exit code 125
/// follows the cron / GNU coreutils convention for "missed /
/// skipped"; stderr carries the deadline + receipt timestamp so
/// the operator can see *how* late we were on the Results page.
async fn publish_skipped(
    _client: &async_nats::Client,
    pc_id: &str,
    cmd: &Command,
    deadline: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let lateness = now - deadline;
    let stderr = format!(
        "skipped: starting deadline expired {} ago (deadline {}, received {})",
        humantime::format_duration(
            lateness
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(0))
        ),
        deadline,
        now,
    );
    let result = ExecResult {
        request_id: cmd.request_id.clone(),
        pc_id: pc_id.to_string(),
        exit_code: 125,
        stdout: String::new(),
        stderr,
        started_at: now,
        finished_at: now,
        manifest_id: Some(cmd.id.clone()),
    };
    let outbox_dir = default_paths::data_dir().join("outbox");
    let path = outbox::enqueue(&outbox_dir, &result)?;
    info!(
        request_id = %cmd.request_id,
        exit_code = 125,
        outbox = %path.display(),
        "synthetic skipped-result enqueued to outbox",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .timestamp_opt(1_700_000_000 + secs, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn now_strictly_before_deadline_runs() {
        assert!(!should_skip_for_deadline(at(100), at(99)));
    }

    #[test]
    fn now_one_second_before_deadline_runs() {
        assert!(!should_skip_for_deadline(at(100), at(99)));
    }

    #[test]
    fn now_exactly_at_deadline_still_runs() {
        // Boundary: == is the *last* allowed instant. Lets a cron
        // tick fire at the exact starting_deadline without spuriously
        // skipping on clock-rounding.
        assert!(!should_skip_for_deadline(at(100), at(100)));
    }

    #[test]
    fn now_one_second_past_deadline_skips() {
        assert!(should_skip_for_deadline(at(100), at(101)));
    }

    #[test]
    fn now_long_past_deadline_skips() {
        assert!(should_skip_for_deadline(at(100), at(86400)));
    }
}
