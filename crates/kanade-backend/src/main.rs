mod api;
mod audit;
mod auth;
mod projector;
mod scheduler;
mod web;

#[cfg(target_os = "windows")]
mod service;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::Parser;
use kanade_shared::config::{LogSection, load_backend_config};
use kanade_shared::default_paths;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(
    name = "kanade-backend",
    about = "kanade backend (axum + SQLite projector)",
    version
)]
struct Cli {
    /// Path to backend.toml. When unset, the backend looks at
    /// $KANADE_BACKEND_CONFIG, then `<config_dir>/backend.toml` (see
    /// kanade_shared::default_paths::config_dir).
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Top-level entry point.
///
/// Mirrors kanade-agent's main: on Windows we probe the Service
/// Control Manager first and run as a real service if SCM is
/// driving us; otherwise we fall through to console mode. Non-
/// Windows targets always run in console mode.
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
    runtime.block_on(run_backend())
}

pub(crate) async fn run_backend() -> Result<()> {
    // Config first so the tracing init can honor [log] path / level
    // / keep_days. v0.24: prior to this the backend's tracing layer
    // was stdout-only, which meant the Windows service (no console)
    // wrote zero log lines anywhere on disk — invisible crashes.
    let cli = Cli::parse();
    let cfg_path = default_paths::find_config(
        cli.config.as_deref(),
        "KANADE_BACKEND_CONFIG",
        "backend.toml",
    )?;
    let cfg =
        load_backend_config(&cfg_path).with_context(|| format!("load config from {cfg_path:?}"))?;

    // _log_guard must outlive the program — tracing_appender's
    // non_blocking writer flushes its pending buffer on Drop.
    let _log_guard = init_tracing(&cfg.log)
        .with_context(|| format!("init tracing from [log] in {cfg_path:?}"))?;

    info!(
        bind = %cfg.server.bind,
        nats = %cfg.nats.url,
        db = %cfg.db.sqlite_path,
        log_path = %cfg.log.path,
        log_keep_days = cfg.log.keep_days,
        "starting kanade-backend",
    );

    // SQLite open + migrate. Ensure the parent directory exists so
    // `create_if_missing(true)` actually has a folder to drop the file
    // into when `db.sqlite_path` points at a fresh install-layout
    // location like `C:\ProgramData\Kanade\data\backend.db`.
    let sqlite_path = PathBuf::from(&cfg.db.sqlite_path);
    if let Some(parent) = sqlite_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create sqlite parent {parent:?}"))?;
    }
    let sqlite_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cfg.db.sqlite_path))
        .with_context(|| format!("parse sqlite path {}", cfg.db.sqlite_path))?
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(sqlite_opts)
        .await
        .context("open sqlite pool")?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("run migrations")?;
    info!("sqlite migrations applied");

    // NATS connect + JetStream context. The shared helper picks up
    // $KANADE_NATS_TOKEN when set and attaches it as the bearer
    // token; same env name + same semantics across agent / backend /
    // CLI so a single fleet-wide secret covers all three.
    let nats = kanade_shared::nats_client::connect(&cfg.nats.url).await?;
    info!("connected to NATS");
    let jetstream = async_nats::jetstream::new(nats.clone());

    // Self-bootstrap every JetStream resource the fleet expects.
    // Idempotent — re-running just re-acks existing resources —
    // so a fresh NATS server, a partial setup, or a server restart
    // all converge to the same state without operator action.
    kanade_shared::bootstrap::ensure_jetstream_resources(&jetstream)
        .await
        .context("ensure_jetstream_resources")?;
    info!("jetstream resources ready");

    // Projectors run in the background; if either exits the backend keeps
    // serving HTTP (read-only API stays useful even if a stream is missing).
    //
    // v0.14: the inventory projector is gone — inventory facts now
    // arrive through the results projector (via Manifest.inventory
    // hint + ExecResult.manifest_id). HwInventory wire is retired.
    {
        let pool = pool.clone();
        let js = jetstream.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::results::run(js, pool).await {
                error!(error = %e, "results projector exited");
            }
        });
    }
    {
        let pool = pool.clone();
        let js = jetstream.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::audit::run(js, pool).await {
                error!(error = %e, "audit projector exited");
            }
        });
    }
    {
        let pool = pool.clone();
        let nats_client = nats.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::heartbeat::run(nats_client, pool).await {
                error!(error = %e, "heartbeat projector exited");
            }
        });
    }
    // v0.30 / PR α: project agent `events.started.*.*` into the new
    // `running_runs` table. Pair with the results projector above —
    // events inserts on script-spawn, results UPDATEs `finished_at`
    // on ExecResult arrival. Drives the SPA Activity Running tab.
    {
        let pool = pool.clone();
        let js = jetstream.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::events::run(js, pool).await {
                error!(error = %e, "events projector exited");
            }
        });
    }

    let app_state = api::AppState {
        pool: pool.clone(),
        nats,
        jetstream,
    };

    // Scheduler runs alongside the projectors; if it can't init (no
    // schedules KV, bad cron, etc.) the backend keeps serving HTTP.
    {
        let s = app_state.clone();
        tokio::spawn(async move {
            if let Err(e) = scheduler::run(s).await {
                error!(error = %e, "scheduler exited");
            }
        });
    }

    let app = api::router(app_state)
        .layer(axum::middleware::from_fn(auth::verify))
        .layer(TraceLayer::new_for_http());

    let listener = TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("bind {}", cfg.server.bind))?;
    info!(bind = %cfg.server.bind, "axum serving");
    axum::serve(listener, app).await.context("axum serve")?;
    Ok(())
}

/// Build the tracing subscriber: stdout (useful in foreground /
/// `cargo run` mode) + a daily-rotated file appender pointed at
/// `[log] path`. `RUST_LOG`, if set, overrides `[log] level`.
/// Returns the appender's `WorkerGuard`, which the caller must
/// keep alive — its Drop flushes the non-blocking writer's
/// pending buffer. v0.24: previously the backend used a stdout-
/// only `tracing_subscriber::fmt()` init, which meant the Windows
/// service (no console) wrote zero log lines anywhere on disk.
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
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("backend");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("log");

    std::fs::create_dir_all(dir).with_context(|| format!("create log dir {dir:?}"))?;

    let appender = tracing_appender::rolling::Builder::new()
        .filename_prefix(stem)
        .filename_suffix(ext)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .max_log_files(log.keep_days)
        .build(dir)
        .with_context(|| format!("build rolling appender at {dir:?}"))?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
        .try_init();

    Ok(Some(guard))
}
