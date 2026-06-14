mod check_cache;
mod commands;
mod config_supervisor;
mod env_gate;
mod groups;
mod heartbeat;
mod host_perf;
mod job_object;
mod job_tail;
mod live_tail;
mod log_tail;
mod logs;
mod ping;
mod process;
mod process_perf;
mod self_update;

// KLP (SPEC §2.12) is Windows-only in this PR — Linux UDS lands
// in a follow-up. Compiling the module on non-Windows would just
// emit dead-code warnings (the listener's call sites are all
// Windows-gated), so the simplest gate is the mod declaration
// itself. Cross-platform unit tests (framing, etc.) move with the
// module; CI on Linux/macOS skips them, but the production target
// is Windows-only so coverage stays meaningful.
#[cfg(target_os = "windows")]
mod klp;

mod command_replay;
mod events_outbox;
mod local_scheduler;
mod nats_retry;
mod obs_outbox;
mod outbox;
mod script_cache;
mod staleness;

#[cfg(target_os = "windows")]
mod client_shortcut;
#[cfg(target_os = "windows")]
mod cwd_expand;
#[cfg(target_os = "windows")]
mod process_as_user;
#[cfg(target_os = "windows")]
mod service;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use kanade_shared::config::{LogSection, load_agent_config};
use kanade_shared::{default_paths, subject};
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "kanade-agent",
    about = "Windows endpoint management agent (kanade)",
    version
)]
struct Cli {
    /// Path to agent.toml. When unset, the agent looks at
    /// $KANADE_AGENT_CONFIG, then `<config_dir>/agent.toml` (see
    /// kanade_shared::default_paths::config_dir).
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Top-level entry point.
///
/// On Windows, we first try to attach to the Service Control Manager.
/// If that succeeds we run as a real Windows service (service.rs
/// owns the tokio runtime for the lifetime of the service); if it
/// fails with `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` (Win32 1063),
/// we fall through to console mode — convenient for `cargo run` and
/// for manual debugging.
///
/// On non-Windows targets we always run in console mode.
fn main() -> Result<()> {
    // #582: boot sentinel is the VERY first thing — before the service
    // dispatcher, config, tracing, or NATS — so a binary that
    // crash-loops on boot (the failure mode that took the backend down
    // via #573) is rolled back to last-good instead of looping forever.
    // It's sync and needs only the data dir + current exe + version.
    // On rollback we exit(64); whether under the SCM (a start failure
    // the failure-action recovers) or console, that relaunches into the
    // restored binary.
    if let Ok(exe) = std::env::current_exe() {
        use kanade_shared::boot_sentinel::{BootDecision, BootSentinel, DEFAULT_MAX_ATTEMPTS};
        let sentinel = BootSentinel::new(&default_paths::data_dir(), exe, AGENT_VERSION);
        if let BootDecision::RolledBack { from } = sentinel.check_on_boot(DEFAULT_MAX_ATTEMPTS) {
            eprintln!(
                "boot sentinel: {from} crash-looped on boot — rolled back to last-good; \
                 exiting (64) for restart"
            );
            std::process::exit(64);
        }
    }

    #[cfg(target_os = "windows")]
    {
        match service::try_run_as_service() {
            Ok(()) => return Ok(()),
            Err(e) if service::is_not_under_scm(&e) => {
                // Not started by SCM — fall through to console mode.
            }
            Err(e) => return Err(anyhow::anyhow!("service dispatcher failed: {e}")),
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(run_agent())
}

/// Run the agent's tokio main loop. Called either from console
/// mode (directly from `main`) or from inside the Windows service
/// entry point (see [`service::run_service`]).
pub(crate) async fn run_agent() -> Result<()> {
    // (boot sentinel check runs in `main()` before the service
    // dispatcher — see there.)

    // Load config first so the tracing init can honor [log] path / level
    // / keep_days. Early errors from this load fall back to stderr.
    let cli = Cli::parse();
    let cfg_path =
        default_paths::find_config(cli.config.as_deref(), "KANADE_AGENT_CONFIG", "agent.toml")?;
    let cfg =
        load_agent_config(&cfg_path).with_context(|| format!("load config from {cfg_path:?}"))?;

    // `_log_guard` must outlive the program — `tracing_appender::non_blocking`
    // writes asynchronously, so the worker thread flushes on its Drop.
    let _log_guard = init_tracing(&cfg.log)
        .with_context(|| format!("init tracing from [log] in {cfg_path:?}"))?;

    cleanup_stale_upgrade_artifacts();

    info!(
        pc_id = %cfg.agent.id,
        nats_url = %cfg.agent.nats_url,
        version = AGENT_VERSION,
        log_path = %cfg.log.path,
        log_keep_days = cfg.log.keep_days,
        "starting kanade-agent",
    );

    // v0.26: build the staleness tracker BEFORE the NATS client so we
    // can hand its event_callback closure to the connect path. Every
    // subsequent `Event::Connected` (initial handshake + every
    // reconnect) stamps the tracker's `last_connected_at`. The
    // tracker itself owns no task — `staleness()` is a pure read.
    let staleness_tracker = staleness::Tracker::new();
    let client = kanade_shared::nats_client::connect_with_event_callback(
        &cfg.agent.nats_url,
        staleness_tracker.on_event(),
    )
    .await?;
    info!("connected to NATS");

    let cmd_all = client.subscribe(subject::COMMANDS_ALL).await?;
    let cmd_self = client
        .subscribe(subject::commands_pc(&cfg.agent.id))
        .await?;
    info!(
        commands_all = subject::COMMANDS_ALL,
        commands_self = %subject::commands_pc(&cfg.agent.id),
        "subscribed",
    );

    let pc_id = cfg.agent.id.clone();

    // Sprint 6: every fleet-wide knob (heartbeat cadence, inventory
    // cadence / jitter / enabled, target_version) is now sourced
    // from the agent_config KV bucket and watched live. The
    // supervisor publishes the resolved EffectiveConfig on a watch
    // channel; heartbeat / inventory / self_update subscribe.
    let cfg_rx = config_supervisor::spawn(client.clone(), pc_id.clone(), staleness_tracker.clone());

    tokio::spawn(heartbeat::heartbeat_loop(
        client.clone(),
        pc_id.clone(),
        AGENT_VERSION.to_string(),
        cfg_rx.clone(),
    ));
    // v0.40 Part 1: host-wide perf snapshot publisher. Runs on its
    // own cadence (default 60 s) so the slightly heavier host-wide
    // sysinfo refresh stays out of the 30 s heartbeat loop. Pre-0.40
    // backends without a host_perf projector simply ignore the
    // traffic, so the agent can be upgraded ahead of the backend.
    tokio::spawn(host_perf::host_perf_loop(
        client.clone(),
        pc_id.clone(),
        cfg_rx.clone(),
    ));
    // v0.41 / Phase 2: per-process telemetry. The loop itself is
    // always spawned, but it stays quiet until the operator flips
    // `process_perf_enabled=true` on this PC's agent_config row
    // (and `process_perf_expires_at` is still in the future). When
    // the deadline passes the loop auto-stops publishing without
    // needing the operator to come back and unset the flag.
    tokio::spawn(process_perf::process_perf_loop(
        client.clone(),
        pc_id.clone(),
        cfg_rx.clone(),
    ));
    // Keep the end-user Client App's all-users Start-Menu shortcut
    // (Start-Menu label + WinRT toast sender name) in step with the
    // operator-configured `client_display_name`. Event-driven off the
    // same config watch; the PowerShell re-stamp only fires when the
    // resolved name actually changes (see client_shortcut docs).
    #[cfg(target_os = "windows")]
    client_shortcut::spawn(cfg_rx.clone());
    tokio::spawn(self_update::run(
        client.clone(),
        pc_id.clone(),
        AGENT_VERSION.to_string(),
        cfg_rx.clone(),
        staleness_tracker.clone(),
    ));
    // #582: we're past early boot (config, tracing, NATS connect,
    // subscriptions, every loop spawn). After a short healthy-uptime
    // grace, confirm to the boot sentinel so this version is promoted
    // to last-good and any pending swap sentinel clears. A crash before
    // the grace elapses leaves the sentinel armed, so the next boot
    // re-counts toward rollback.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        if let Ok(exe) = std::env::current_exe() {
            let sentinel = kanade_shared::boot_sentinel::BootSentinel::new(
                &default_paths::data_dir(),
                exe,
                AGENT_VERSION,
            );
            if let Err(e) = sentinel.confirm_healthy() {
                tracing::warn!(error = %e, "boot sentinel: confirm_healthy failed");
            }
        }
    });
    tokio::spawn(logs::serve(
        client.clone(),
        pc_id.clone(),
        std::path::PathBuf::from(&cfg.log.path),
        staleness_tracker.clone(),
    ));
    // Live job-tail responder: serves `job.tail.<pc_id>` from the
    // in-memory live registry so the SPA can poll a running job's
    // stdout/stderr (same UX as the agent-log auto-refresh, scoped
    // to one job). No persistence — purely the in-flight ring buffer.
    tokio::spawn(job_tail::serve(
        client.clone(),
        pc_id.clone(),
        staleness_tracker.clone(),
    ));
    // v0.38 / #133: active ping responder. Independent of the
    // periodic heartbeat loop so an operator's "ping" round-trips
    // in single-digit ms instead of waiting up to ~30 s for the
    // next scheduled tick.
    tokio::spawn(ping::serve(
        client.clone(),
        pc_id.clone(),
        AGENT_VERSION.to_string(),
        std::env::var("COMPUTERNAME")
            .ok()
            .or_else(|| std::env::var("HOSTNAME").ok()),
        Some(std::env::consts::OS.to_string()),
        staleness_tracker.clone(),
    ));

    // KLP listener (SPEC §2.12) — Windows Named Pipe today, Linux
    // UDS in a follow-up. The detached JoinHandle is intentional:
    // the foundation PR has no graceful-shutdown path and the
    // listener should run for the agent's full lifetime.
    //
    // The state evaluator runs on a 30 s cadence in its own task
    // and publishes StateSnapshots to a watch channel that the
    // KLP listener fans out to subscribers (state.subscribe).
    // Seed the watch with `eval_once` synchronously so the first
    // `state.snapshot` call returns real data without waiting for
    // a tick.
    // #290: operator-defined health-check results flow from the
    // command path into this sink and out via the KLP StateSnapshot.
    // Constructed unconditionally (the command-path writer is
    // cross-platform); only the Windows KLP evaluator reads it.
    // Persisted to data_dir so the Health tab survives an agent
    // restart and shows last-known status while offline.
    let check_sink =
        check_cache::CheckSink::load(default_paths::data_dir().join("check_results.json"));
    // KLP Phase E: process-wide notification broadcast. Created before
    // the listener so the same sender feeds both the KLP
    // `ListenerContext` (per-connection forwarders derive receivers from
    // it) and the `notify_bus` task spawned once `groups_rx` exists.
    // The initial receiver is dropped — receivers are minted on demand
    // by `notifications.subscribe`; until then `send` is a no-op.
    #[cfg(target_os = "windows")]
    let notif_tx =
        tokio::sync::broadcast::channel::<kanade_shared::ipc::notifications::Notification>(
            klp::notify_bus::BROADCAST_CAPACITY,
        )
        .0;
    #[cfg(target_os = "windows")]
    {
        let initial_snapshot = klp::state::eval_once(
            &pc_id,
            AGENT_VERSION,
            &cfg_rx.borrow(),
            klp::state::client_online(&client),
            &check_sink.checks(),
        );
        let (state_tx, state_rx) = tokio::sync::watch::channel(initial_snapshot);
        tokio::spawn(klp::state::eval_loop(
            state_tx,
            cfg_rx.clone(),
            pc_id.clone(),
            AGENT_VERSION.to_string(),
            client.clone(),
            check_sink.clone(),
        ));
        let _klp_handle = klp::server::spawn(klp::server::ListenerContext {
            pc_id: std::sync::Arc::from(pc_id.as_str()),
            agent_version: std::sync::Arc::from(AGENT_VERSION),
            config_rx: cfg_rx.clone(),
            state_rx,
            log_path: std::path::PathBuf::from(&cfg.log.path),
            nats: client.clone(),
            notif_tx: notif_tx.clone(),
        });
    }

    // Group membership: Sprint 5 moves this from agent.toml (per-box
    // local config) to a server-managed KV bucket. The manager reads
    // `agent_groups.{pc_id}` from JetStream KV, spawns one
    // `commands.group.<name>` subscriber per current group, and reacts
    // to KV updates by adding / dropping subscriptions live.
    if !cfg.agent.groups.is_empty() {
        tracing::warn!(
            local_groups = ?cfg.agent.groups,
            "agent.toml::[agent] groups is deprecated; use `kanade agent groups set` instead — local value is ignored",
        );
    }
    // v0.22.1: dedup cache shared between core sub (live online
    // path) and the JetStream replay consumer (reconnect catch-up).
    // Either path can be the first to deliver a given Command's
    // request_id; the second arrival is dropped.
    let dedup = commands::shared_dedup_cache();

    // #210: OBJECT_SCRIPTS-backed manifest scripts. Constructed
    // once here (cheap Clone — jetstream::Context is Arc-internal)
    // and threaded into every dispatch path (groups subs + replay +
    // live sub + local scheduler) so they all share one cache
    // directory.
    let script_cache = script_cache::ScriptCache::new(
        async_nats::jetstream::new(client.clone()),
        default_paths::data_dir().join("script_cache"),
    );

    // v0.24: groups::spawn returns a watch::Receiver<Vec<String>>
    // carrying the current membership list. `local_scheduler`
    // subscribes to it so `runs_on: agent` schedules targeting a
    // group reflect membership changes without waiting for the
    // next schedule edit.
    let (groups_rx, _groups_handle) = groups::spawn(
        client.clone(),
        pc_id.clone(),
        dedup.clone(),
        staleness_tracker.clone(),
        script_cache.clone(),
        check_sink.clone(),
    );

    // KLP Phase E (live push): subscribe to the membership-filtered
    // `notifications.{all|group.X|pc.Y}` subjects and re-broadcast each
    // incoming notification to every connected Client App. Follows
    // `groups_rx` so the subject set tracks group membership, just like
    // command_replay. Windows-only (the whole KLP module is).
    #[cfg(target_os = "windows")]
    klp::notify_bus::spawn(client.clone(), pc_id.clone(), groups_rx.clone(), notif_tx);

    // Reconnect catch-up: durable consumer on STREAM_EXEC that
    // replays the latest retained Command per subject. See
    // `crates/kanade-agent/src/command_replay.rs` for the flow.
    // #483: it follows `groups_rx` so the durable's server-side
    // filter_subjects track group membership (and group Commands
    // never reach non-members).
    command_replay::spawn(
        client.clone(),
        pc_id.clone(),
        dedup.clone(),
        staleness_tracker.clone(),
        script_cache.clone(),
        check_sink.clone(),
        groups_rx.clone(),
    );
    // v0.24: file-based outbox for ExecResult publishes. Every
    // result the agent produces is persisted under `outbox/<rid>.json`
    // first; a background drain task publishes via JetStream and
    // deletes on PubAck. Survives agent crashes + broker-down
    // periods longer than the async-nats client buffer.
    let outbox_dir = default_paths::data_dir().join("outbox");
    let _outbox_handle = outbox::spawn_drain(client.clone(), outbox_dir.clone());
    // v0.30 / PR α' unified: parallel outbox for `EventStarted`
    // lifecycle events (script-spawn time). Same atomic write +
    // drain pattern as the ExecResult outbox; separate directory so
    // existing v0.24 ExecResult files keep working unchanged across
    // the upgrade.
    let events_outbox_dir = default_paths::data_dir().join("events-outbox");
    let _events_outbox_handle =
        events_outbox::spawn_drain(client.clone(), events_outbox_dir.clone());
    // Issue #246: per-PC observability event outbox + drain. Distinct
    // from `events-outbox/` above (which carries `EventStarted`
    // lifecycle events) — `obs-outbox/` carries the timeline
    // `ObsEvent`s a script emits via `emit.type: events` manifests.
    let obs_outbox_dir = default_paths::data_dir().join("obs-outbox");
    let _obs_outbox_handle = obs_outbox::spawn_drain(client.clone(), obs_outbox_dir.clone());
    // v0.23: schedules marked `runs_on: agent` tick locally so the
    // agent keeps firing even when the broker is unreachable. See
    // `crates/kanade-agent/src/local_scheduler.rs` for the flow.
    let completions_path = default_paths::data_dir().join("local_completions.json");
    local_scheduler::spawn(
        client.clone(),
        pc_id.clone(),
        completions_path,
        groups_rx,
        staleness_tracker.clone(),
        script_cache.clone(),
        check_sink.clone(),
    );

    let _ = tokio::join!(
        commands::command_loop(
            client.clone(),
            pc_id.clone(),
            dedup.clone(),
            staleness_tracker.clone(),
            cmd_all,
            script_cache.clone(),
            check_sink.clone(),
        ),
        commands::command_loop(
            client.clone(),
            pc_id.clone(),
            dedup.clone(),
            staleness_tracker.clone(),
            cmd_self,
            script_cache.clone(),
            check_sink.clone(),
        ),
    );

    Ok(())
}

/// Build the tracing subscriber: stdout (useful in foreground /
/// `cargo run` mode) + a daily-rotated file appender pointed at
/// `log.path`. `RUST_LOG`, if set, overrides `log.level`. Returns
/// the appender's `WorkerGuard`, which the caller must keep alive
/// — its Drop flushes the non-blocking writer's pending buffer.
fn init_tracing(log: &LogSection) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| log.level.clone().into());

    // keep_days = 0 → opt out of file logging entirely (stdout only).
    if log.keep_days == 0 {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
            .try_init();
        return Ok(None);
    }

    let path = Path::new(&log.path);
    let dir = path
        .parent()
        .with_context(|| format!("[log] path '{}' has no parent dir", log.path))?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("agent");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("log");

    std::fs::create_dir_all(dir).with_context(|| format!("create log dir {dir:?}"))?;

    let appender = tracing_appender::rolling::Builder::new()
        .filename_prefix(stem)
        .filename_suffix(ext)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .max_log_files(log.keep_days)
        .build(dir)
        .context("build rolling file appender")?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false),
        )
        .try_init();

    Ok(Some(guard))
}

/// Remove `<exe>.old` / `<exe>.new` left over from the previous
/// self-update cycle. `.old` is the previous-version exe, no longer
/// loaded; `.new` would only exist if a swap was interrupted before
/// the final rename (the in-place exe is still valid in that case).
/// Either way, removal here keeps the install dir tidy and stops
/// stale binaries from accumulating across upgrade cycles.
fn cleanup_stale_upgrade_artifacts() {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = current.parent() else {
        return;
    };
    let Some(exe_name) = current.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    for suffix in ["old", "new"] {
        let path = exe_dir.join(format!("{exe_name}.{suffix}"));
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(_) => tracing::info!(?path, suffix, "removed stale upgrade artifact"),
            Err(e) => {
                tracing::warn!(?path, suffix, error = %e, "couldn't remove stale upgrade artifact")
            }
        }
    }
}
