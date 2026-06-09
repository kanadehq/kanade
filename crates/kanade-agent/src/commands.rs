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
use uuid::Uuid;

use crate::outbox;
use crate::process::{ExecOutcome, apply_jitter, run_command_with_kill};
use crate::script_cache::ScriptCache;
use crate::staleness::{StalenessDecision, Tracker, decide as staleness_decide};

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
    staleness: Tracker,
    mut sub: async_nats::Subscriber,
    script_cache: ScriptCache,
    check_sink: crate::check_cache::CheckSink,
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
        let staleness = staleness.clone();
        let script_cache = script_cache.clone();
        let check_sink = check_sink.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_command(
                client,
                pc_id,
                cmd,
                cur,
                sta,
                staleness,
                script_cache,
                check_sink,
            )
            .await
            {
                error!(error = %e, "command handler failed");
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_command(
    client: async_nats::Client,
    pc_id: String,
    mut cmd: Command,
    script_current: Option<Store>,
    script_status: Option<Store>,
    staleness: Tracker,
    script_cache: ScriptCache,
    check_sink: crate::check_cache::CheckSink,
) -> Result<()> {
    // Spec §2.6 Layer 2: version-pinning + revoke check + v0.26
    // staleness policy. The order matters:
    //   1. staleness gate first — if the policy is `strict` and the
    //      agent's broker view is too old, refuse to run before
    //      consulting any KV (because a stale KV read would be
    //      misleading). `cached` / `unchecked` flow straight through.
    //   2. version-pin + revoke checks (unchanged from v0.22.1).
    let staleness_now = staleness.staleness(&client);
    match staleness_decide(&cmd.staleness, staleness_now) {
        StalenessDecision::Proceed => {}
        StalenessDecision::Skip { observed, allowed } => {
            warn!(
                cmd_id = %cmd.id,
                request_id = %cmd.request_id,
                observed_s = observed.as_secs(),
                allowed_s = allowed.as_secs(),
                "skip: staleness policy (mode=strict) exceeded — broker view too old",
            );
            return publish_staleness_skipped(&pc_id, &cmd, observed, allowed).await;
        }
    }

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
            // #271: publish a synthetic skipped-result so the
            // `executions` row this command belongs to transitions
            // out of `pending` rather than rotting forever and
            // inflating the /api/jobs `実行中` counter. Same shape
            // as the Layer 1 staleness / deadline-expired skips.
            return publish_version_mismatch_skipped(&pc_id, &cmd, &expected).await;
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
            // #271: same fix as the version-mismatch path — emit a
            // synthetic result so the executions row can reach
            // `completed` instead of stranding `pending`.
            return publish_revoked_skipped(&pc_id, &cmd).await;
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

    // #210: resolve OBJECT_SCRIPTS-backed scripts just in time.
    // Backend's exec.rs builds Commands with `script: ""` +
    // `script_object: Some(key)` + `script_object_sha256: Some(d)`
    // when the manifest uses `script_object:`. Fill `cmd.script`
    // here so the rest of the dispatch (run_command_with_kill,
    // stdout/stderr capture, exec_id event emission) stays
    // identical to the inline-script path.
    if cmd.script.is_empty()
        && let Some(key) = cmd.script_object.as_deref()
    {
        let sha = cmd.script_object_sha256.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Command {request_id} has script_object={key} but no script_object_sha256 \
                 — wire builder bug",
                request_id = cmd.request_id,
            )
        })?;
        match script_cache.resolve(key, sha).await {
            Ok(body) => {
                debug!(
                    cmd_id = %cmd.id,
                    request_id = %cmd.request_id,
                    %key,
                    sha256 = %sha,
                    size = body.len(),
                    "script_object resolved",
                );
                cmd.script = body;
            }
            Err(e) => {
                warn!(
                    cmd_id = %cmd.id,
                    request_id = %cmd.request_id,
                    %key,
                    sha256 = %sha,
                    error = %e,
                    "script_object resolve failed — aborting run",
                );
                return Err(e);
            }
        }
    }

    // Jitter BEFORE we log "executing command" and stamp `started_at`,
    // so the log line, the recorded duration (`finished_at -
    // started_at`), and the events.started lifecycle event all reflect
    // the real execution start — not the per-PC fleet-stagger wait. It
    // previously sat inside `run_command_with_kill`, i.e. after the
    // timestamp, so a 5-min jitter inflated the dashboard 所要時間 well
    // past the job's `timeout:` even though the script itself finished
    // in seconds (the timeout arm only ever bounded the post-jitter
    // execution), and "executing command" logged minutes before
    // anything ran. apply_jitter emits its own "applying jitter" line,
    // so the wait is still visible in the logs. The "実行中" view
    // likewise no longer lights up during the jitter wait.
    apply_jitter(&cmd).await;

    info!(
        cmd_id = %cmd.id,
        request_id = %cmd.request_id,
        version = %cmd.version,
        exec_id = ?cmd.exec_id,
        "executing command",
    );
    let started_at = chrono::Utc::now();
    // v0.30 / PR α' unified: mint result_id once at the top of
    // handle_command and thread it through both the EventStarted
    // (script-spawn lifecycle event) and the ExecResult so the
    // backend's UPSERT against `execution_results.result_id`
    // coalesces both into a single row regardless of arrival order.
    let result_id = Uuid::new_v4().to_string();

    // Emit `events.started.<exec_id>.<pc_id>` BEFORE child spawn
    // when the Command carries an exec_id (= deployment from
    // `kanade exec` or scheduler tick, not ad-hoc `kanade run`).
    // Goes through the file outbox so offline / mid-broker-outage
    // runs still surface on reconnect. Outbox enqueue failures are
    // warn-logged but never abort the run — losing the start
    // lifecycle event means the row in execution_results is
    // backfilled from the ExecResult side with default version etc.
    if let Some(exec_id) = cmd.exec_id.as_deref() {
        let event = kanade_shared::wire::EventStarted {
            result_id: result_id.clone(),
            request_id: cmd.request_id.clone(),
            exec_id: exec_id.to_string(),
            pc_id: pc_id.clone(),
            started_at,
            manifest_id: cmd.id.clone(),
            version: cmd.version.clone(),
        };
        let events_outbox_dir = default_paths::data_dir().join("events-outbox");
        match crate::events_outbox::enqueue(&events_outbox_dir, &event) {
            Ok(p) => debug!(
                result_id = %result_id,
                events_outbox = %p.display(),
                "started event enqueued (drain task delivers via JetStream)",
            ),
            Err(e) => warn!(
                error = %e,
                result_id = %result_id,
                "events_outbox enqueue failed; in-flight view will not show this row until ExecResult lands",
            ),
        }
    }

    let outcome = run_command_with_kill(&client, &cmd).await?;
    let finished_at = chrono::Utc::now();

    let (exit_code, stdout, stderr, status_note) = match outcome {
        ExecOutcome::Completed {
            exit_code,
            stdout,
            stderr,
        } => (exit_code, stdout, stderr, None),
        ExecOutcome::Killed { stdout, stderr } => {
            let eid = cmd.exec_id.as_deref().unwrap_or("?");
            (
                -1,
                stdout,
                stderr,
                Some(format!("killed by remote signal (kill.{eid})")),
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

    // #290: if this job is an operator-defined health check, map its
    // result into the KLP `StateSnapshot.checks` for the Client App's
    // Health tab. Done HERE — before the `emit` branch below can blank
    // `stdout` — so a check always reads the real script output even
    // if a (validation-prevented) `emit:` somehow rode along. On a
    // clean exit the status comes from the stdout object; a non-zero
    // exit records `Unknown` (with the exit code + stderr) rather than
    // leaving a stale `Ok`, so a persistently-crashing check can't read
    // as healthy.
    if let Some(check_hint) = &cmd.check {
        let check = if exit_code == 0 {
            crate::check_cache::build_check(check_hint, &stdout)
        } else {
            crate::check_cache::build_check_failed(check_hint, exit_code, &stderr)
        };
        check_sink.record(check);
    }

    // Issue #246: if the manifest is an event emitter, parse stdout
    // as NDJSON `ObsEvent` and route each line to obs_outbox.
    // Stdout is then DROPPED from the ExecResult — the timeline
    // data lives in `obs_events` and re-shipping it via
    // `execution_results.stdout` would multiply ~50/day/PC of
    // noise into a table designed for one row per script run.
    //
    // Only fires on a clean exit (`exit_code == 0`). A failed run
    // keeps stdout in the result so operators can see what went
    // wrong on the Activity page — partial event lines from a
    // crashed script are more confusing than absent ones.
    let stdout = if exit_code == 0
        && matches!(
            cmd.emit.as_ref().map(|e| e.kind),
            Some(kanade_shared::manifest::EmitKind::Events),
        ) {
        forward_obs_events(stdout, pc_id.clone()).await;
        // Don't ship the NDJSON itself in stdout; the events are
        // now in obs-outbox and the Activity row's stdout would
        // just duplicate them.
        String::new()
    } else {
        stdout
    };

    let result = ExecResult {
        // v0.30 / PR α' unified: same `result_id` value used in the
        // matching EventStarted above. Backend UPSERTs against
        // `execution_results.result_id`, so the events.started
        // insert and this ExecResult update coalesce into a single
        // row regardless of arrival order.
        result_id: result_id.clone(),
        request_id: cmd.request_id.clone(),
        // v0.29 / Issue #19: forward `Command.exec_id` so the backend
        // projector can increment `executions.success_count` /
        // `failure_count` and the upcoming /api/executions endpoint
        // can list per-PC results for one deployment.
        exec_id: cmd.exec_id.clone(),
        pc_id: pc_id.clone(),
        exit_code,
        stdout,
        stderr,
        started_at,
        finished_at,
        // #227: outbox-drain side fills these in when stdout / stderr
        // exceeds the inline threshold and gets offloaded to
        // OBJECT_RESULT_OUTPUT. Stays None at enqueue time so the
        // outbox file on disk preserves the full bytes (drain task
        // re-runs the overflow check on every iteration — idempotent
        // re-upload to same key).
        stdout_object: None,
        stderr_object: None,
        // Forward `Command.id` (the manifest's id, e.g. "inventory-hw"),
        // NOT `Command.exec_id` (a per-deploy UUID). The backend's
        // results projector uses this to look up the manifest's
        // `inventory:` hint and upsert `inventory_facts` rows.
        manifest_id: Some(cmd.id.clone()),
    };
    let outbox_dir = default_paths::data_dir().join("outbox");
    let path = outbox::enqueue(&outbox_dir, &result)?;
    debug!(
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

/// Issue #246 — parse each non-empty stdout line as `ObsEvent`
/// and enqueue to `obs-outbox`. Lines that fail to decode warn +
/// skip (don't fail the rest of the batch). The caller has already
/// ensured `cmd.emit.kind == Events` and `exit_code == 0`, so this
/// only runs when the manifest explicitly opts in AND the script
/// succeeded.
///
/// Each line is parsed in isolation; one bad line doesn't poison
/// the others. Empty lines (the natural NDJSON trailing newline +
/// any blank lines a script accidentally emits) are skipped
/// silently.
///
/// Gemini #249 high: the parse + per-line `enqueue` (tmp write +
/// rename) is synchronous file I/O on the Tokio runtime thread.
/// For a 50-event poll that's ~50 ms of blocked executor time per
/// agent — measurable on a busy host. Wrap the whole batch in
/// `spawn_blocking` so the executor stays free; the moved values
/// (`stdout`, `pc_id`) are owned strings the closure can carry.
async fn forward_obs_events(stdout: String, pc_id: String) {
    use kanade_shared::wire::ObsEvent;
    let obs_outbox_dir = default_paths::data_dir().join("obs-outbox");
    // Hoist the `create_dir_all` out of the per-event hot path
    // (Gemini #249 medium). One syscall per batch instead of per
    // event.
    if let Err(e) = crate::obs_outbox::ensure_outbox_dir(&obs_outbox_dir) {
        warn!(error = %e, "obs: ensure_outbox_dir failed; aborting forward");
        return;
    }
    let pc_id_log = pc_id.clone();
    let (ok, bad) = tokio::task::spawn_blocking(move || {
        let mut ok = 0usize;
        let mut bad = 0usize;
        for (i, raw) in stdout.lines().enumerate() {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut event: ObsEvent = match serde_json::from_str(trimmed) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        line_no = i + 1,
                        error = %e,
                        "obs: stdout line is not a valid ObsEvent JSON; skipping",
                    );
                    bad += 1;
                    continue;
                }
            };
            // Scripts that emit a hard-coded `pc_id` (the docs
            // example does this) would race with PC renames.
            // Override with the agent's authoritative value —
            // `obs.<pc_id>` subject and the backend UNIQUE-key
            // column both need to match.
            event.pc_id = pc_id.clone();
            if let Err(e) = crate::obs_outbox::enqueue(&obs_outbox_dir, &event) {
                warn!(
                    line_no = i + 1,
                    error = %e,
                    "obs: enqueue to outbox failed; line dropped",
                );
                bad += 1;
            } else {
                ok += 1;
            }
        }
        (ok, bad)
    })
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "obs: forwarder task panicked / cancelled");
        (0, 0)
    });
    info!(ok, bad, pc_id = %pc_id_log, "obs: forwarded NDJSON stdout to obs-outbox");
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

/// v0.26: Synthesise an ExecResult for "Layer 2 strict staleness
/// exceeded — agent couldn't verify it's running the latest version
/// because the broker view is too old." Exit code 127 is reserved
/// for this case. Agent-side skip exit codes are partitioned:
///
/// | Code | Meaning                                | Helper                            |
/// |------|----------------------------------------|-----------------------------------|
/// | 124  | Layer 2 version-pin mismatch (#271)    | `publish_version_mismatch_skipped`|
/// | 125  | deadline_at expired                    | `publish_skipped`                 |
/// | 126  | Layer 2 revoked (#271)                 | `publish_revoked_skipped`         |
/// | 127  | Layer 1 staleness (mode=strict)        | `publish_staleness_skipped`       |
///
/// The stderr carries the observed staleness window + the
/// configured allowance so the operator sees on the Results page
/// why the fire was suppressed and what they'd need to change to
/// allow it.
async fn publish_staleness_skipped(
    pc_id: &str,
    cmd: &Command,
    observed: std::time::Duration,
    allowed: std::time::Duration,
) -> Result<()> {
    let now = chrono::Utc::now();
    let stderr = format!(
        "skipped: staleness policy (mode=strict) exceeded — agent has been disconnected for {}, max allowed {}",
        humantime::format_duration(observed),
        humantime::format_duration(allowed),
    );
    let result = ExecResult {
        result_id: Uuid::new_v4().to_string(),
        request_id: cmd.request_id.clone(),
        exec_id: cmd.exec_id.clone(),
        pc_id: pc_id.to_string(),
        exit_code: 127,
        stdout: String::new(),
        stderr,
        started_at: now,
        finished_at: now,
        stdout_object: None,
        stderr_object: None,
        manifest_id: Some(cmd.id.clone()),
    };
    let outbox_dir = default_paths::data_dir().join("outbox");
    let path = outbox::enqueue(&outbox_dir, &result)?;
    info!(
        request_id = %cmd.request_id,
        exit_code = 127,
        outbox = %path.display(),
        "staleness-skip result enqueued to outbox",
    );
    Ok(())
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
        result_id: Uuid::new_v4().to_string(),
        request_id: cmd.request_id.clone(),
        exec_id: cmd.exec_id.clone(),
        pc_id: pc_id.to_string(),
        exit_code: 125,
        stdout: String::new(),
        stderr,
        started_at: now,
        finished_at: now,
        stdout_object: None,
        stderr_object: None,
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

/// #271: Synthesise an ExecResult for "Layer 2 version-pin
/// mismatch — incoming Command's version doesn't match the
/// `script_current.<id>` KV value the backend just published."
/// Exit code 124 distinguishes this from the sibling skip paths
/// (see the table on [`publish_staleness_skipped`]).
///
/// Without this synthetic result, the matching `executions` row
/// stays at `status='pending'` forever and the `/api/jobs` `実行中`
/// counter monotonically inflates across every stale skip — the
/// bug #271 documents from the 0.43.1 → 0.43.2 bump.
async fn publish_version_mismatch_skipped(
    pc_id: &str,
    cmd: &Command,
    expected: &str,
) -> Result<()> {
    let now = chrono::Utc::now();
    let stderr = format!(
        "skipped: version-pin mismatch — script_current[{}] = {expected}, command brought {}",
        cmd.id, cmd.version,
    );
    let result = ExecResult {
        result_id: Uuid::new_v4().to_string(),
        request_id: cmd.request_id.clone(),
        exec_id: cmd.exec_id.clone(),
        pc_id: pc_id.to_string(),
        exit_code: 124,
        stdout: String::new(),
        stderr,
        started_at: now,
        finished_at: now,
        stdout_object: None,
        stderr_object: None,
        manifest_id: Some(cmd.id.clone()),
    };
    let outbox_dir = default_paths::data_dir().join("outbox");
    let path = outbox::enqueue(&outbox_dir, &result)?;
    info!(
        request_id = %cmd.request_id,
        exit_code = 124,
        outbox = %path.display(),
        "version-mismatch skip result enqueued to outbox",
    );
    Ok(())
}

/// #271: Synthesise an ExecResult for "Layer 2 revoked — the
/// operator marked this manifest revoked via
/// `script_status.<id> = revoked` before the agent received this
/// Command." Exit code 126 distinguishes this from the sibling
/// skip paths (see the table on [`publish_staleness_skipped`]).
///
/// Same `executions`-row rationale as
/// [`publish_version_mismatch_skipped`].
async fn publish_revoked_skipped(pc_id: &str, cmd: &Command) -> Result<()> {
    let now = chrono::Utc::now();
    let stderr = format!(
        "skipped: command was revoked (script_status[{}] = revoked)",
        cmd.id,
    );
    let result = ExecResult {
        result_id: Uuid::new_v4().to_string(),
        request_id: cmd.request_id.clone(),
        exec_id: cmd.exec_id.clone(),
        pc_id: pc_id.to_string(),
        exit_code: 126,
        stdout: String::new(),
        stderr,
        started_at: now,
        finished_at: now,
        stdout_object: None,
        stderr_object: None,
        manifest_id: Some(cmd.id.clone()),
    };
    let outbox_dir = default_paths::data_dir().join("outbox");
    let path = outbox::enqueue(&outbox_dir, &result)?;
    info!(
        request_id = %cmd.request_id,
        exit_code = 126,
        outbox = %path.display(),
        "revoked skip result enqueued to outbox",
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
