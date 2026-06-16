//! `jobs.*` method handlers (SPEC §2.12.5 / §2.12.11).
//!
//! - `jobs.list` — return every manifest carrying a `client:` block,
//!   optionally narrowed to one [`JobCategory`], mapped into the
//!   [`UserInvokableJob`] wire shape the Client App's three job tabs
//!   (アップデート / 困ったとき / catalog) render.
//! - `jobs.execute` — run a user-invokable job. Looks the manifest up
//!   by id, refuses anything without a `client:` block
//!   (`Unauthorized` per SPEC §2.12.4), mints a `run_id`, spawns the
//!   run, and returns the `run_id` immediately. The run streams
//!   `jobs.progress` pushes (Running → Completed/Failed/Killed) on the
//!   connection's push channel.
//! - `jobs.kill` — request termination of a `run_id` started ON THIS
//!   connection (cross-connection kill → `Unauthorized`). Publishes
//!   `subject::kill(run_id)`; the run's terminal `jobs.progress`
//!   (status = Killed) follows asynchronously once the child exits.
//!
//! `jobs.subscribe` / `jobs.unsubscribe` (an explicit progress
//! subscription) and incremental stdout/stderr streaming land in a
//! follow-up — today `jobs.execute` pushes progress directly on the
//! connection it was called on (the client always wants progress for a
//! run it explicitly started), and the terminal push carries the full
//! captured output rather than live chunks. `script_object` jobs
//! (Object Store bodies) are also a follow-up; `jobs.execute` only
//! runs inline-`script:` jobs for now.
//!
//! # Catalog source
//!
//! The agent reads the manifest catalog straight from the
//! `BUCKET_JOBS` KV at call time rather than from a cached snapshot,
//! so adding / removing a manifest's `client:` block (or editing its
//! `name`) takes effect on the client's next `jobs.list` / `jobs.execute`
//! without an agent restart — SPEC §2.1's "Agent 側で manifest を必ず 再
//! lookup" rule. These are cold, user-initiated paths (a tab tap, a
//! button press), so the extra KV round-trip is immaterial.
//!
//! The pure [`build_job_list`] / [`build_command`] /
//! [`outcome_to_progress`] helpers are split out from the KV + process
//! glue so they can be unit-tested without a live NATS or a real child
//! process.

use chrono::Utc;
use futures::TryStreamExt;
use kanade_shared::ipc::envelope::RpcNotification;
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::jobs::{
    JobProgress, JobsExecuteParams, JobsExecuteResult, JobsKillParams, JobsKillResult,
    JobsListParams, JobsListResult, RunStatus, UserInvokableJob,
};
use kanade_shared::ipc::method;
use kanade_shared::kv::BUCKET_JOBS;
use kanade_shared::manifest::Manifest;
use kanade_shared::wire::Command;
use kanade_shared::{ExecResult, default_paths, subject};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use super::super::connection::ConnectionState;
use super::system::HandlerResult;
use crate::outbox;
use crate::process::{ExecOutcome, run_command_with_kill};

/// `jobs.list` — list the user-invokable job catalog for the Client
/// App, optionally filtered to a single tab's category.
///
/// Reads `BUCKET_JOBS` on demand (see module docs). A connectivity
/// failure opening or scanning the bucket surfaces as
/// [`ErrorKind::InternalError`]; the client retries on the next tab
/// switch.
pub async fn handle_jobs_list(
    conn: &ConnectionState,
    params: JobsListParams,
) -> HandlerResult<JobsListResult> {
    // `nats` is always wired in production (the listener calls
    // `with_nats`); a `None` here only happens in a unit test that
    // forgot to, so treat it as an internal wiring bug, not a client
    // error.
    let client = conn.nats.as_ref().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "jobs.list: NATS client not wired into the connection",
        )
    })?;

    let js = async_nats::jetstream::new(client.clone());
    let kv = js.get_key_value(BUCKET_JOBS).await.map_err(|e| {
        warn!(error = %e, "jobs.list: failed to open BUCKET_JOBS");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.list: open jobs catalog: {e}"),
        )
    })?;

    // keys() failing is a connectivity-level error (broker hiccup),
    // distinct from "no jobs registered" (an empty key set) — mirror
    // local_scheduler::collect_jobs and surface it rather than
    // returning an empty catalog the client would read as "nothing
    // to run".
    let keys = kv.keys().await.map_err(|e| {
        warn!(error = %e, "jobs.list: BUCKET_JOBS keys() failed");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.list: scan jobs catalog: {e}"),
        )
    })?;
    // A fault mid-iteration (broker hiccup after the cursor opened)
    // is a connectivity error, NOT "no jobs" — propagate it so the
    // client retries instead of rendering an empty catalog. Swallowing
    // it with `unwrap_or_default()` would contradict the keys()
    // handling just above.
    let keys: Vec<String> = keys.try_collect().await.map_err(|e| {
        warn!(error = %e, "jobs.list: BUCKET_JOBS key stream faulted mid-iteration");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.list: stream jobs catalog: {e}"),
        )
    })?;

    // Fetch every manifest concurrently: `jobs.list` has to read the
    // whole BUCKET_JOBS (it can't tell which entries are user-invokable
    // without parsing them), so a fleet with dozens of jobs would pay N
    // sequential round-trips if fetched in a loop. A single corrupt /
    // unreadable entry is skipped (logged) rather than sinking the
    // whole listing — same tolerance the scheduler's catalog walk uses.
    let manifests: Vec<Manifest> = futures::future::join_all(keys.into_iter().map(|k| {
        let kv = kv.clone();
        async move {
            match kv.get(&k).await {
                Ok(Some(bytes)) => match serde_json::from_slice::<Manifest>(&bytes) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        warn!(key = %k, error = %e, "jobs.list: skipping unparseable manifest");
                        None
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    warn!(key = %k, error = %e, "jobs.list: skipping unreadable manifest");
                    None
                }
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();

    Ok(build_job_list(&manifests, params.category))
}

/// Pure mapping + filtering: manifests → the `jobs.list` wire result.
///
/// Keeps only manifests carrying a `client:` block, maps each to a
/// [`UserInvokableJob`], applies the optional category filter, and
/// sorts by display name so the catalog renders in a stable order
/// regardless of KV key iteration order.
pub fn build_job_list(
    manifests: &[Manifest],
    filter: Option<kanade_shared::ipc::jobs::JobCategory>,
) -> JobsListResult {
    let mut items: Vec<UserInvokableJob> = manifests
        .iter()
        .filter_map(manifest_to_job)
        .filter(|j| filter.is_none_or(|c| j.category == c))
        .collect();
    // Stable, human-meaningful order: display name, then id as the
    // tiebreaker so two jobs sharing a name don't render
    // nondeterministically.
    items.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then_with(|| a.id.cmp(&b.id))
    });
    JobsListResult { items }
}

/// Map one manifest to its catalog row, or `None` when it carries no
/// `client:` block (i.e. it's an operator-only job).
///
/// The `client:` block's required fields (`name`, `category`) are
/// guaranteed present by serde at parse time, so this is a
/// straight field-for-field projection — no defaulting needed.
fn manifest_to_job(m: &Manifest) -> Option<UserInvokableJob> {
    let client = m.client.as_ref()?;
    Some(UserInvokableJob {
        id: m.id.clone(),
        display_name: client.name.clone(),
        display_description: client.description.clone(),
        icon: client.icon.clone(),
        category: client.category,
        version: m.version.clone(),
        // Per-user run history is minted by `jobs.execute` (a
        // follow-up PR); until then every row is "never run by you".
        last_run: None,
    })
}

// ---------- jobs.execute ----------

/// `jobs.execute` — run a user-invokable job and stream its progress.
///
/// Looks the manifest up by id (re-lookup at fire time, SPEC §2.1),
/// refuses anything without a `client:` block (`Unauthorized`), mints
/// a `run_id`, spawns the run, and returns the `run_id` immediately.
/// The spawned task pushes `jobs.progress` (Running → terminal) on the
/// connection's push channel; the run is detached so it keeps going if
/// the client disconnects mid-run (a half-finished install shouldn't be
/// abandoned).
pub async fn handle_jobs_execute(
    conn: &mut ConnectionState,
    params: JobsExecuteParams,
) -> HandlerResult<JobsExecuteResult> {
    let client = conn
        .nats
        .as_ref()
        .ok_or_else(|| {
            RpcError::new(
                ErrorKind::InternalError,
                "jobs.execute: NATS client not wired into the connection",
            )
        })?
        .clone();

    let manifest = fetch_manifest(&client, &params.id).await?;

    // SPEC §2.12.4: only `client:` (user-invokable) jobs may be run
    // from the Client App. An operator-only job → Unauthorized, never
    // MethodNotFound (the method exists; this caller just can't run
    // THAT job).
    if manifest.client.is_none() {
        return Err(RpcError::new(
            ErrorKind::Unauthorized,
            format!(
                "job '{}' is not user-invokable (no client: block)",
                params.id
            ),
        ));
    }

    let run_id = Uuid::new_v4().to_string();
    let request_id = Uuid::new_v4().to_string();
    let cmd = build_command(&manifest, &run_id, &request_id)?;

    // Record the run BEFORE spawning so a near-instant `jobs.kill`
    // can't race ahead of the registry insert.
    conn.register_run(run_id.clone());

    let push_tx = conn.push_tx.clone();
    let pc_id = conn.pc_id.clone();
    let spawned_run_id = run_id.clone();
    tokio::spawn(run_job(client, cmd, spawned_run_id, push_tx, pc_id));

    Ok(JobsExecuteResult { run_id })
}

/// Fetch one manifest from `BUCKET_JOBS` by id. `InvalidParams` when
/// the id isn't a valid KV key, `NotFound` when the key is absent,
/// `InternalError` on a KV / decode failure.
async fn fetch_manifest(client: &async_nats::Client, id: &str) -> Result<Manifest, RpcError> {
    // Validate the client-supplied id up front: anything that isn't a
    // legal job id (= NATS KV key) would make `kv.get` fail with a
    // confusing InternalError. Catch it here as InvalidParams.
    if !valid_job_id(id) {
        return Err(RpcError::new(
            ErrorKind::InvalidParams,
            format!("job id '{id}' is not a valid job id (expected [A-Za-z0-9_.-])"),
        ));
    }
    let js = async_nats::jetstream::new(client.clone());
    let kv = js.get_key_value(BUCKET_JOBS).await.map_err(|e| {
        warn!(error = %e, "jobs.execute: failed to open BUCKET_JOBS");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.execute: open jobs catalog: {e}"),
        )
    })?;
    let bytes = kv
        .get(id)
        .await
        .map_err(|e| {
            warn!(key = %id, error = %e, "jobs.execute: KV get failed");
            RpcError::new(
                ErrorKind::InternalError,
                format!("jobs.execute: read job '{id}': {e}"),
            )
        })?
        .ok_or_else(|| RpcError::new(ErrorKind::NotFound, format!("job '{id}' not found")))?;
    serde_json::from_slice::<Manifest>(&bytes).map_err(|e| {
        warn!(key = %id, error = %e, "jobs.execute: manifest decode failed");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.execute: decode job '{id}': {e}"),
        )
    })
}

/// `true` if `id` is a legal manifest id / NATS KV key: a non-empty
/// slug of `[A-Za-z0-9_.-]`. Pure so the gate is unit-testable.
fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Build the wire [`Command`] the run path executes from a manifest.
/// Genuinely pure (no I/O, no id minting) so it's deterministically
/// unit-testable — the caller passes both ids. `run_id` doubles as the
/// `exec_id` so `run_command_with_kill` subscribes to
/// `subject::kill(run_id)` and `jobs.kill` can target it; `request_id`
/// is the per-run correlation id stamped on the wire Command.
///
/// Inline-`script:` only for now — `script_object` jobs (Object Store
/// bodies fetched via the agent's script cache) are a follow-up, and
/// `script_file` is operator-CLI-side and never reaches the agent.
pub fn build_command(
    manifest: &Manifest,
    run_id: &str,
    request_id: &str,
) -> Result<Command, RpcError> {
    if manifest.execute.script_object.is_some() {
        return Err(RpcError::new(
            ErrorKind::InvalidParams,
            format!(
                "job '{}' uses script_object, which is not yet runnable via KLP jobs.execute",
                manifest.id
            ),
        ));
    }
    let script = manifest
        .execute
        .script
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RpcError::new(
                ErrorKind::InvalidParams,
                format!("job '{}' has no inline script to run", manifest.id),
            )
        })?
        .to_string();
    // `.max(1)`: a sub-second timeout (e.g. `500ms`) truncates to 0
    // under `as_secs()`, and a 0 `timeout_secs` is ambiguous to the run
    // path (no-timeout vs immediate-timeout). Floor any positive
    // duration at 1s — a user-invokable action measured in milliseconds
    // is not a real config.
    let timeout_secs = humantime::parse_duration(&manifest.execute.timeout)
        .map_err(|e| {
            RpcError::new(
                ErrorKind::InvalidParams,
                format!("job '{}' has an invalid timeout: {e}", manifest.id),
            )
        })?
        .as_secs()
        .max(1);
    Ok(Command {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        request_id: request_id.to_string(),
        // run_id == exec_id: the kill subject the run subscribes to.
        exec_id: Some(run_id.to_string()),
        shell: manifest.execute.shell.into(),
        script,
        script_object: None,
        script_object_sha256: None,
        timeout_secs,
        // User-fired one-shot — no fan-out, so no jitter / deadline.
        jitter_secs: None,
        run_as: manifest.execute.run_as,
        cwd: manifest.execute.cwd.clone(),
        deadline_at: None,
        staleness: manifest.staleness.clone(),
        // A user-invokable action's stdout drives the progress display,
        // not inventory/check/emit projection — don't forward those
        // hints (the run path would otherwise try to project them).
        emit: None,
        check: None,
        // #219: collect IS forwarded (unlike emit/check) — a
        // `collect:` + `client:` job exists precisely so an end user can
        // trigger a collection from the Client App; `run_job` bundles +
        // uploads the listed files on success.
        collect: manifest.collect.clone(),
        // #418 Phase 4: a KLP-fired one-shot has no schedule behind it,
        // so no on_failure.retry policy — the user re-runs from the app.
        retry: None,
    })
}

/// Spawned per-run task: push `Running`, run the child, push the
/// terminal `jobs.progress`, and publish the `ExecResult` to the
/// backend. Best-effort — every push tolerates a closed channel
/// (client gone) and the run still completes.
async fn run_job(
    client: async_nats::Client,
    cmd: Command,
    run_id: String,
    push_tx: mpsc::Sender<Vec<u8>>,
    pc_id: String,
) {
    push_progress(
        &push_tx,
        JobProgress {
            run_id: run_id.clone(),
            status: RunStatus::Running,
            stdout_chunk: None,
            stderr_chunk: None,
            exit_code: None,
        },
    )
    .await;

    let started_at = Utc::now();
    let outcome = run_command_with_kill(&client, &cmd, None).await;
    let finished_at = Utc::now();

    // Derive both the client progress AND the (exit_code, stdout,
    // stderr) the ExecResult records — for the ran case AND the
    // never-spawned case, so EVERY jobs.execute attempt lands on the
    // operator Activity page (#478), including a spawn failure.
    let (terminal, exit_code, stdout, stderr) = match &outcome {
        Ok(o) => {
            let (code, out, err) = outcome_to_result_parts(&cmd, o);
            (outcome_to_progress(run_id.clone(), o), code, out, err)
        }
        Err(e) => {
            warn!(run_id = %run_id, pc_id = %pc_id, error = %e, "jobs.execute: run failed to start");
            let msg = with_note("", &format!("agent failed to start the job: {e}"));
            (
                JobProgress {
                    run_id: run_id.clone(),
                    status: RunStatus::Failed,
                    stdout_chunk: None,
                    stderr_chunk: Some(msg.clone()),
                    exit_code: Some(-1),
                },
                -1,
                String::new(),
                msg,
            )
        }
    };
    debug!(run_id = %run_id, pc_id = %pc_id, status = ?terminal.status, "jobs.execute: run finished");
    push_progress(&push_tx, terminal).await;

    // #478: record the run on the backend so operators see
    // user-initiated jobs on the Activity page (audit trail), via the
    // same outbox → JetStream path a normal NATS-driven run uses.
    let result = build_exec_result(
        &cmd,
        &pc_id,
        exit_code,
        stdout,
        stderr,
        started_at,
        finished_at,
    );
    enqueue_exec_result(result);
}

/// Map a finished outcome to the `(exit_code, stdout, stderr)` the
/// ExecResult records. Pure / unit-testable. Synthetic outcomes (kill /
/// timeout) carry exit `-1`; annotate stderr so an operator reading
/// `-1` on the Activity page knows which it was.
fn outcome_to_result_parts(cmd: &Command, outcome: &ExecOutcome) -> (i32, String, String) {
    match outcome {
        ExecOutcome::Completed {
            exit_code,
            stdout,
            stderr,
        } => (*exit_code, stdout.clone(), stderr.clone()),
        ExecOutcome::Killed { stdout, stderr } => (
            -1,
            stdout.clone(),
            with_note(stderr, "killed by the user via KLP jobs.kill"),
        ),
        ExecOutcome::Timeout { stdout, stderr } => (
            -1,
            stdout.clone(),
            with_note(stderr, &format!("timed out after {}s", cmd.timeout_secs)),
        ),
    }
}

/// Assemble the [`ExecResult`] for a KLP run. Pure / unit-testable.
///
/// `exec_id` is `None`: a KLP run has no parent deployment, so this is
/// an ad-hoc result (like `kanade run`) and must NOT carry the
/// `run_id` as an `exec_id` — that would point the backend's
/// `executions`-aggregate projector at a row that doesn't exist
/// (#478). `cmd.exec_id` stays `Some(run_id)` for `jobs.kill`; only the
/// RESULT decouples. (`result_id` is a fresh per-result UUID — the only
/// non-deterministic field, and not asserted in tests.)
fn build_exec_result(
    cmd: &Command,
    pc_id: &str,
    exit_code: i32,
    stdout: String,
    stderr: String,
    started_at: chrono::DateTime<Utc>,
    finished_at: chrono::DateTime<Utc>,
) -> ExecResult {
    ExecResult {
        result_id: Uuid::new_v4().to_string(),
        request_id: cmd.request_id.clone(),
        exec_id: None,
        pc_id: pc_id.to_string(),
        exit_code,
        stdout,
        stderr,
        started_at,
        finished_at,
        // The outbox drain offloads oversized output to the object
        // store on its own; None at enqueue keeps the full bytes on disk.
        stdout_object: None,
        stderr_object: None,
        manifest_id: Some(cmd.id.clone()),
        // #219: set by the collect step (PR2) for a `collect:` job; None
        // otherwise. The KLP path is exactly how a `collect:`+`client:`
        // job fired from the Client App gets bundled.
        collect_object: None,
    }
}

/// Enqueue a finished run's [`ExecResult`] onto the outbox. Offloaded
/// to the blocking pool because `outbox::enqueue` does synchronous file
/// I/O (`create_dir_all` / `write` / `rename`) that would otherwise
/// block an async runtime thread. Fire-and-forget + best-effort: an
/// enqueue failure is logged, never fails the run (the client already
/// got its terminal progress).
fn enqueue_exec_result(result: ExecResult) {
    let manifest_id = result.manifest_id.clone().unwrap_or_default();
    let pc_id = result.pc_id.clone();
    tokio::task::spawn_blocking(move || {
        let outbox_dir = default_paths::data_dir().join("outbox");
        match outbox::enqueue(&outbox_dir, &result) {
            Ok(path) => debug!(
                manifest_id = %manifest_id,
                pc_id = %pc_id,
                outbox = %path.display(),
                "jobs.execute: ExecResult enqueued (operator visibility, #478)",
            ),
            Err(e) => warn!(
                manifest_id = %manifest_id,
                pc_id = %pc_id,
                error = %e,
                "jobs.execute: ExecResult outbox enqueue failed (run still completed)",
            ),
        }
    });
}

/// Append a `[KLP] <note>` line to a captured stderr (or use it alone
/// when stderr is blank), so an operator seeing exit `-1` knows whether
/// the run was killed or timed out. Trailing whitespace is trimmed
/// first so a process's trailing newline doesn't produce a blank line
/// before the note.
fn with_note(stderr: &str, note: &str) -> String {
    let trimmed = stderr.trim_end();
    if trimmed.is_empty() {
        format!("[KLP] {note}")
    } else {
        format!("{trimmed}\n[KLP] {note}")
    }
}

/// Per-chunk cap on a terminal progress push's stdout/stderr. The KLP
/// framing layer rejects frames over 1 MiB (SPEC §2.12.2); a job that
/// dumps megabytes of output would otherwise produce a frame the
/// writer can't deliver, silently dropping the terminal status. Cap
/// each chunk at 256 KiB so stdout + stderr + the JSON envelope stay
/// comfortably under the frame limit. A truncated run still reports its
/// real status + exit_code; only the tail of the output is dropped (the
/// full output also lives in the agent log).
const MAX_PROGRESS_CHUNK_BYTES: usize = 256 * 1024;

/// Map a finished [`ExecOutcome`] to its terminal `jobs.progress`.
/// Pure so the status / exit-code mapping is unit-testable without a
/// real child. Empty stdout/stderr are omitted (`None`) per the wire
/// contract — strict JS clients reject `null` chunk strings.
pub fn outcome_to_progress(run_id: String, outcome: &ExecOutcome) -> JobProgress {
    use std::borrow::Cow;
    // Borrow the captured strings (they can be hundreds of KiB) — only
    // the Timeout arm needs an owned stderr to append its note, so it's
    // the only `Cow::Owned`.
    let (status, exit_code, stdout, stderr): (RunStatus, i32, Cow<str>, Cow<str>) = match outcome {
        ExecOutcome::Completed {
            exit_code,
            stdout,
            stderr,
        } => {
            let status = if *exit_code == 0 {
                RunStatus::Completed
            } else {
                RunStatus::Failed
            };
            (
                status,
                *exit_code,
                Cow::Borrowed(stdout),
                Cow::Borrowed(stderr),
            )
        }
        // Synthetic outcomes carry exit_code -1; the `status` field is
        // what distinguishes "you stopped it" from "it errored".
        ExecOutcome::Killed { stdout, stderr } => (
            RunStatus::Killed,
            -1,
            Cow::Borrowed(stdout),
            Cow::Borrowed(stderr),
        ),
        // Timeout folds into `Failed` — the wire `RunStatus` has no
        // Timeout variant — so a bare client would render it
        // indistinguishably from a non-zero exit. Stamp a note onto
        // stderr so the Client App can show WHY it failed ("timed out"
        // vs "errored"), since the script's own stderr usually says
        // nothing about being killed at the deadline.
        ExecOutcome::Timeout { stdout, stderr } => {
            const NOTE: &str =
                "⏱ ジョブがタイムアウトしました（manifest の timeout で打ち切られました）";
            let stderr = if stderr.trim().is_empty() {
                NOTE.to_string()
            } else {
                format!("{stderr}\n{NOTE}")
            };
            (
                RunStatus::Failed,
                -1,
                Cow::Borrowed(stdout),
                Cow::Owned(stderr),
            )
        }
    };
    JobProgress {
        run_id,
        status,
        stdout_chunk: (!stdout.is_empty()).then(|| cap_chunk(&stdout)),
        stderr_chunk: (!stderr.is_empty()).then(|| cap_chunk(&stderr)),
        exit_code: Some(exit_code),
    }
}

/// Truncate a progress chunk to [`MAX_PROGRESS_CHUNK_BYTES`] on a UTF-8
/// char boundary, appending a marker so the user sees output was
/// dropped. Returns the input unchanged when it's already under cap.
fn cap_chunk(s: &str) -> String {
    if s.len() <= MAX_PROGRESS_CHUNK_BYTES {
        return s.to_string();
    }
    let mut end = MAX_PROGRESS_CHUNK_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated: output exceeded 256 KiB]", &s[..end])
}

/// Encode a `jobs.progress` notification and push it on the
/// connection's writer channel. Best-effort: a serialise failure is
/// logged and dropped, and a closed channel (client disconnected)
/// just means progress stops — the run itself is unaffected.
async fn push_progress(push_tx: &mpsc::Sender<Vec<u8>>, progress: JobProgress) {
    let notif = match RpcNotification::new(method::JOBS_PROGRESS, &progress) {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %e, "jobs.progress: failed to encode notification");
            return;
        }
    };
    let body = match serde_json::to_vec(&notif) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "jobs.progress: failed to serialise frame");
            return;
        }
    };
    if push_tx.send(body).await.is_err() {
        debug!("jobs.progress: push channel closed (client gone)");
    }
}

// ---------- jobs.kill ----------

/// `jobs.kill` — request termination of a run started ON THIS
/// connection. SPEC §2.12.4 forbids cross-connection kill, so a
/// `run_id` this connection never started → `Unauthorized` (NOT
/// `NotFound`, which would leak whether the id exists on another
/// connection). Publishes `subject::kill(run_id)`; the run's terminal
/// `jobs.progress` (status = Killed) follows once the child exits.
pub async fn handle_jobs_kill(
    conn: &ConnectionState,
    params: JobsKillParams,
) -> HandlerResult<JobsKillResult> {
    if !conn.owns_run(&params.run_id) {
        return Err(RpcError::new(
            ErrorKind::Unauthorized,
            format!("run '{}' was not started on this connection", params.run_id),
        ));
    }
    let client = conn.nats.as_ref().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "jobs.kill: NATS client not wired into the connection",
        )
    })?;
    client
        .publish(subject::kill(&params.run_id), bytes::Bytes::new())
        .await
        .map_err(|e| {
            warn!(run_id = %params.run_id, error = %e, "jobs.kill: publish failed");
            RpcError::new(
                ErrorKind::InternalError,
                format!("jobs.kill: publish kill signal: {e}"),
            )
        })?;
    Ok(JobsKillResult {
        requested_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::ipc::jobs::JobCategory;
    use kanade_shared::manifest::{ClientHint, Execute, ExecuteShell};
    use kanade_shared::wire::{RunAs, Staleness};

    /// Build a manifest fixture. Pass `client: Some((name, category))`
    /// for a user-invokable job, `None` for an operator-only one.
    fn manifest(id: &str, client: Option<(&str, JobCategory)>) -> Manifest {
        Manifest {
            id: id.into(),
            version: "1.0.0".into(),
            description: None,
            execute: Execute {
                shell: ExecuteShell::Powershell,
                script: Some("echo hi".into()),
                script_file: None,
                script_object: None,
                timeout: "30s".into(),
                run_as: RunAs::default(),
                cwd: None,
            },
            require_approval: false,
            inventory: None,
            emit: None,
            check: None,
            collect: None,
            staleness: Staleness::default(),
            client: client.map(|(name, category)| ClientHint {
                name: name.into(),
                description: None,
                category,
                icon: None,
            }),
            tags: Vec::new(),
            origin: None,
        }
    }

    #[test]
    fn lists_only_client_jobs() {
        let manifests = [
            manifest("inv-hw", None),
            manifest(
                "chrome-update",
                Some(("Chrome を更新", JobCategory::SoftwareUpdate)),
            ),
            manifest("check-bitlocker", None),
        ];
        let result = build_job_list(&manifests, None);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].id, "chrome-update");
        assert_eq!(result.items[0].display_name, "Chrome を更新");
        assert_eq!(result.items[0].category, JobCategory::SoftwareUpdate);
        assert!(result.items[0].last_run.is_none());
    }

    #[test]
    fn category_filter_narrows_to_one_tab() {
        let manifests = [
            manifest(
                "chrome-update",
                Some(("Chrome", JobCategory::SoftwareUpdate)),
            ),
            manifest("fix-teams", Some(("Teams 修復", JobCategory::Troubleshoot))),
            manifest("install-slack", Some(("Slack", JobCategory::Catalog))),
        ];
        let only_troubleshoot = build_job_list(&manifests, Some(JobCategory::Troubleshoot));
        assert_eq!(only_troubleshoot.items.len(), 1);
        assert_eq!(only_troubleshoot.items[0].id, "fix-teams");
    }

    #[test]
    fn empty_when_no_client_jobs() {
        let manifests = [manifest("inv-hw", None), manifest("inv-sw", None)];
        let result = build_job_list(&manifests, None);
        assert!(result.items.is_empty());
    }

    #[test]
    fn maps_all_client_fields() {
        // Full projection incl. the optional description + icon.
        let mut m = manifest("fix-teams", Some(("Teams 修復", JobCategory::Troubleshoot)));
        if let Some(c) = m.client.as_mut() {
            c.description = Some("重いとき用".into());
            c.icon = Some("brush-cleaning".into());
        }
        let result = build_job_list(std::slice::from_ref(&m), None);
        let row = &result.items[0];
        assert_eq!(row.display_description.as_deref(), Some("重いとき用"));
        assert_eq!(row.icon.as_deref(), Some("brush-cleaning"));
        assert_eq!(row.version, "1.0.0");
    }

    #[test]
    fn items_sorted_by_display_name() {
        let manifests = [
            manifest("z", Some(("Zebra", JobCategory::Catalog))),
            manifest("a", Some(("Apple", JobCategory::Catalog))),
            manifest("m", Some(("Mango", JobCategory::Catalog))),
        ];
        let result = build_job_list(&manifests, None);
        let names: Vec<&str> = result
            .items
            .iter()
            .map(|j| j.display_name.as_str())
            .collect();
        assert_eq!(names, ["Apple", "Mango", "Zebra"]);
    }

    // ---------- build_command ----------

    #[test]
    fn build_command_maps_manifest_fields() {
        let mut m = manifest("fix-teams", Some(("Teams 修復", JobCategory::Troubleshoot)));
        m.execute.run_as = RunAs::User;
        m.execute.cwd = Some("C:/temp".into());
        m.execute.timeout = "90s".into();
        let cmd = build_command(&m, "run-123", "req-9").expect("build");
        assert_eq!(cmd.id, "fix-teams");
        assert_eq!(cmd.version, "1.0.0");
        assert_eq!(cmd.exec_id.as_deref(), Some("run-123")); // run_id == exec_id
        assert_eq!(cmd.request_id, "req-9"); // caller-supplied → deterministic
        assert_eq!(cmd.script, "echo hi");
        assert_eq!(cmd.timeout_secs, 90);
        assert_eq!(cmd.run_as, RunAs::User);
        assert_eq!(cmd.cwd.as_deref(), Some("C:/temp"));
        assert!(cmd.jitter_secs.is_none());
        assert!(cmd.deadline_at.is_none());
        // A user-invokable action isn't an inventory/check/emit producer.
        assert!(cmd.emit.is_none() && cmd.check.is_none());
        assert!(cmd.script_object.is_none());
    }

    #[test]
    fn build_command_rejects_script_object() {
        let mut m = manifest("obj-job", Some(("Obj", JobCategory::Catalog)));
        m.execute.script = None;
        m.execute.script_object = Some("cleanup/1.0.0".into());
        let err = build_command(&m, "r1", "req-1").expect_err("script_object unsupported");
        assert_eq!(err.data.unwrap().kind, ErrorKind::InvalidParams);
    }

    #[test]
    fn build_command_rejects_missing_inline_script() {
        let mut m = manifest("empty", Some(("Empty", JobCategory::Catalog)));
        m.execute.script = None;
        let err = build_command(&m, "r1", "req-1").expect_err("no script");
        let data = err.data.unwrap();
        assert_eq!(data.kind, ErrorKind::InvalidParams);
        assert!(data.detail.contains("no inline script"), "{}", data.detail);
    }

    #[test]
    fn build_command_rejects_bad_timeout() {
        let mut m = manifest("bad", Some(("Bad", JobCategory::Catalog)));
        m.execute.timeout = "not-a-duration".into();
        let err = build_command(&m, "r1", "req-1").expect_err("bad timeout");
        let data = err.data.unwrap();
        assert_eq!(data.kind, ErrorKind::InvalidParams);
        assert!(data.detail.contains("timeout"), "{}", data.detail);
    }

    #[test]
    fn build_command_floors_subsecond_timeout_to_one() {
        // `500ms`.as_secs() == 0, which is an ambiguous timeout to the
        // run path — floor any positive duration at 1s.
        let mut m = manifest("ms", Some(("Ms", JobCategory::Catalog)));
        m.execute.timeout = "500ms".into();
        let cmd = build_command(&m, "r1", "req-1").expect("build");
        assert_eq!(cmd.timeout_secs, 1);
    }

    // ---------- valid_job_id ----------

    #[test]
    fn valid_job_id_accepts_slugs_rejects_junk() {
        for ok in ["chrome-update", "fix_teams.cache", "Job123", "a"] {
            assert!(valid_job_id(ok), "{ok} should be valid");
        }
        for bad in ["", "has space", "wild*", "a>b", "with/slash", "qu?x"] {
            assert!(!valid_job_id(bad), "{bad:?} should be invalid");
        }
    }

    // ---------- cap_chunk ----------

    #[test]
    fn cap_chunk_passes_through_small_output() {
        assert_eq!(cap_chunk("hello"), "hello");
    }

    #[test]
    fn cap_chunk_truncates_oversize_on_char_boundary() {
        // A multibyte string well over the cap: truncation must land on
        // a char boundary (no panic) and carry the marker.
        let big = "あ".repeat(MAX_PROGRESS_CHUNK_BYTES); // 3 bytes each
        let out = cap_chunk(&big);
        assert!(out.len() < big.len(), "must shrink");
        assert!(out.contains("truncated"), "must mark truncation");
        // Round-trips as valid UTF-8 (would have panicked on a bad cut).
        assert!(out.is_char_boundary(out.len()));
    }

    // ---------- outcome_to_progress ----------

    #[test]
    fn outcome_completed_zero_is_completed() {
        let p = outcome_to_progress(
            "r1".into(),
            &ExecOutcome::Completed {
                exit_code: 0,
                stdout: "done".into(),
                stderr: String::new(),
            },
        );
        assert_eq!(p.status, RunStatus::Completed);
        assert_eq!(p.exit_code, Some(0));
        assert_eq!(p.stdout_chunk.as_deref(), Some("done"));
        // Empty stderr is omitted, not Some("").
        assert!(p.stderr_chunk.is_none());
    }

    #[test]
    fn outcome_completed_nonzero_is_failed() {
        let p = outcome_to_progress(
            "r1".into(),
            &ExecOutcome::Completed {
                exit_code: 3,
                stdout: String::new(),
                stderr: "boom".into(),
            },
        );
        assert_eq!(p.status, RunStatus::Failed);
        assert_eq!(p.exit_code, Some(3));
        assert!(p.stdout_chunk.is_none());
        assert_eq!(p.stderr_chunk.as_deref(), Some("boom"));
    }

    #[test]
    fn outcome_killed_maps_to_killed_minus_one() {
        let p = outcome_to_progress(
            "r1".into(),
            &ExecOutcome::Killed {
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        assert_eq!(p.status, RunStatus::Killed);
        assert_eq!(p.exit_code, Some(-1));
    }

    #[test]
    fn outcome_timeout_maps_to_failed_with_note() {
        // Timeout → Failed, but a note is stamped onto stderr so the
        // client can show "timed out" rather than a bare "failed".
        let p = outcome_to_progress(
            "r1".into(),
            &ExecOutcome::Timeout {
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        assert_eq!(p.status, RunStatus::Failed);
        assert_eq!(p.exit_code, Some(-1));
        assert!(
            p.stderr_chunk
                .as_deref()
                .is_some_and(|s| s.contains("タイムアウト")),
            "stderr_chunk should carry the timeout note: {:?}",
            p.stderr_chunk
        );
    }

    #[test]
    fn outcome_timeout_appends_note_after_existing_stderr() {
        let p = outcome_to_progress(
            "r1".into(),
            &ExecOutcome::Timeout {
                stdout: String::new(),
                stderr: "partial output before kill".into(),
            },
        );
        let chunk = p.stderr_chunk.expect("stderr present");
        assert!(chunk.contains("partial output before kill"), "{chunk}");
        assert!(chunk.contains("タイムアウト"), "{chunk}");
    }

    // ---------- jobs.kill authorization ----------

    fn fresh_conn() -> ConnectionState {
        use kanade_shared::ipc::state::StateSnapshot;
        use kanade_shared::wire::EffectiveConfig;
        use std::path::PathBuf;
        use tokio::sync::watch;

        let (_cfg_tx, cfg_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let snapshot = StateSnapshot {
            pc_id: "PC1".into(),
            online: true,
            vpn: "unknown".into(),
            checks: vec![],
            agent_version: "0.0.0".into(),
            target_version: "0.0.0".into(),
        };
        let (_state_tx, state_rx) = watch::channel(snapshot);
        let (push_tx, _push_rx) = mpsc::channel(8);
        ConnectionState::new(
            crate::klp::auth::PeerCredentials {
                user: "DOMAIN\\alice".into(),
                user_sid: "S-1-5-21-1001".into(),
                session_id: 2,
            },
            "PC1".into(),
            "0.0.0".into(),
            cfg_rx,
            state_rx,
            PathBuf::from("agent.log"),
            push_tx,
        )
    }

    #[tokio::test]
    async fn kill_unknown_run_is_unauthorized() {
        // A run_id this connection never started → Unauthorized (NOT
        // NotFound), and we never even reach the NATS publish.
        let conn = fresh_conn();
        let err = handle_jobs_kill(
            &conn,
            JobsKillParams {
                run_id: "never-started".into(),
            },
        )
        .await
        .expect_err("unknown run must be unauthorized");
        assert_eq!(err.data.unwrap().kind, ErrorKind::Unauthorized);
    }

    #[tokio::test]
    async fn kill_owned_run_passes_authorization() {
        // A registered run_id passes the same-connection gate; it then
        // fails at the NATS publish (no client wired in this test),
        // which proves authorization succeeded rather than being
        // rejected up front.
        let mut conn = fresh_conn();
        conn.register_run("run-mine".into());
        let err = handle_jobs_kill(
            &conn,
            JobsKillParams {
                run_id: "run-mine".into(),
            },
        )
        .await
        .expect_err("no nats wired → InternalError after auth passes");
        assert_eq!(err.data.unwrap().kind, ErrorKind::InternalError);
    }

    // ---------- build_exec_result (#478 operator visibility) ----------

    fn cmd_fixture(id: &str) -> Command {
        let m = manifest(id, Some(("Job", JobCategory::Catalog)));
        build_command(&m, "run-1", "req-1").expect("build_command")
    }

    #[test]
    fn exec_result_is_ad_hoc_with_manifest_id() {
        // The KLP run must record as an ad-hoc result (exec_id None) so
        // the backend doesn't try to increment a non-existent
        // `executions` aggregate row keyed by exec_id (#478).
        let cmd = cmd_fixture("fix-teams");
        let now = Utc::now();
        let r = build_exec_result(&cmd, "PC1", 0, "ok".into(), String::new(), now, now);
        assert!(r.exec_id.is_none(), "KLP run must be ad-hoc (exec_id None)");
        assert_eq!(r.manifest_id.as_deref(), Some("fix-teams"));
        assert_eq!(r.pc_id, "PC1");
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.stdout, "ok");
        assert!(!r.result_id.is_empty(), "result_id minted");
    }

    #[test]
    fn result_parts_completed_passes_through() {
        let cmd = cmd_fixture("j");
        let (code, out, err) = outcome_to_result_parts(
            &cmd,
            &ExecOutcome::Completed {
                exit_code: 2,
                stdout: "o".into(),
                stderr: "e".into(),
            },
        );
        assert_eq!((code, out.as_str(), err.as_str()), (2, "o", "e"));
    }

    #[test]
    fn result_parts_killed_and_timeout_annotate_stderr() {
        let cmd = cmd_fixture("j");

        let (code, _out, err) = outcome_to_result_parts(
            &cmd,
            &ExecOutcome::Killed {
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        assert_eq!(code, -1);
        assert!(err.contains("killed"), "{err}");

        let (code, _out, err) = outcome_to_result_parts(
            &cmd,
            &ExecOutcome::Timeout {
                stdout: String::new(),
                stderr: "partial".into(),
            },
        );
        assert_eq!(code, -1);
        // Existing partial output is kept AND the note is appended.
        assert!(
            err.contains("partial") && err.contains("timed out"),
            "{err}"
        );
    }

    #[test]
    fn with_note_appends_or_stands_alone() {
        assert_eq!(with_note("", "x"), "[KLP] x");
        assert_eq!(with_note("   ", "x"), "[KLP] x");
        assert_eq!(with_note("out", "x"), "out\n[KLP] x");
        // Trailing newline is trimmed so there's no blank line before
        // the note.
        assert_eq!(with_note("out\n", "x"), "out\n[KLP] x");
    }
}
