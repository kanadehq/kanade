mod commands;
mod config_supervisor;
mod groups;
mod heartbeat;
mod host_perf;
mod logs;
mod ping;
mod process;
mod process_perf;
mod self_update;

mod command_replay;
mod events_outbox;
mod local_scheduler;
mod nats_retry;
mod outbox;
mod staleness;

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
    tokio::spawn(self_update::run(
        client.clone(),
        AGENT_VERSION.to_string(),
        cfg_rx.clone(),
    ));
    tokio::spawn(logs::serve(
        client.clone(),
        pc_id.clone(),
        std::path::PathBuf::from(&cfg.log.path),
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
    ));

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
    );
    // Reconnect catch-up: durable consumer on STREAM_EXEC that
    // replays the latest retained Command per subject. See
    // `crates/kanade-agent/src/command_replay.rs` for the flow.
    command_replay::spawn(
        client.clone(),
        pc_id.clone(),
        dedup.clone(),
        staleness_tracker.clone(),
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
    );

    let _ = tokio::join!(
        commands::command_loop(
            client.clone(),
            pc_id.clone(),
            dedup.clone(),
            staleness_tracker.clone(),
            cmd_all,
        ),
        commands::command_loop(
            client.clone(),
            pc_id.clone(),
            dedup.clone(),
            staleness_tracker.clone(),
            cmd_self,
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
