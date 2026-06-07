mod api;
mod audit;
mod auth;
mod cleanup;
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
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
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

    // Route panics through tracing so they land in the log file. The
    // default hook only writes to stderr, which a Windows service
    // discards — a panic in a request handler (e.g. jsonwebtoken's
    // CryptoProvider panic on the first JWT mint) would otherwise vanish
    // without a trace, leaving an "endpoint stopped responding" report
    // undiagnosable from the box. hyper still catches per-request panics,
    // so this changes only their visibility, not the crash behaviour.
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `force_capture` (not `capture`) so the backtrace is collected
        // even without RUST_BACKTRACE set — a Windows service has no
        // environment to flip, and the default hook prints the backtrace
        // to the same discarded stderr. line-tables-only debug info (see
        // [profile] in Cargo.toml) keeps the frames meaningful.
        let backtrace = std::backtrace::Backtrace::force_capture();
        error!(panic = %info, %backtrace, "panic");
        default_panic_hook(info);
    }));

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
    // #411: concurrency pragmas. sqlx leaves journal_mode untouched
    // (so a fresh DB runs in `delete` mode — every write takes an
    // exclusive lock that blocks all readers) and defaults to
    // synchronous=FULL + a 5 s busy_timeout. Measured on minipc with a
    // single PC: multi-second single-row INSERTs (up to 7.2 s — past
    // the 5 s busy_timeout, surfacing as `database is locked` to the
    // projectors, which skip the ack and trigger JetStream redelivery
    // storms).
    //   * WAL — readers and the writer no longer block each other,
    //     which is the actual shape of this workload (8-conn pool:
    //     projectors writing while the API/scheduler read).
    //   * synchronous=NORMAL — safe with WAL (power loss can drop the
    //     last commit(s) but never corrupts), and this DB is a
    //     projection that re-derives from JetStream anyway (#389
    //     WipeDb replay), so FULL's per-commit fsync tax buys nothing.
    //   * busy_timeout 30 s — headroom over the worst observed stall
    //     so residual writer-writer contention waits instead of
    //     erroring into the redelivery path.
    let sqlite_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cfg.db.sqlite_path))
        .with_context(|| format!("parse sqlite path {}", cfg.db.sqlite_path))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(30));
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

    // RBAC bootstrap: seed the first admin account if the users table
    // is empty (chicken-and-egg). Reads the password registry-first
    // (HKLM\SOFTWARE\kanade\backend\BootstrapAdminPassword) /
    // env-second ($KANADE_BOOTSTRAP_ADMIN_PASSWORD); a loud warning is
    // logged either way. Without it, the only entry is the static
    // service token / KANADE_AUTH_DISABLE.
    if let Err(e) = api::accounts::seed_bootstrap_admin(&pool).await {
        warn!(error = %e, "bootstrap admin seed failed");
    }

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

    // #389: a wiped projection DB (deploy -WipeDb, manual recovery)
    // leaves the projectors' durable consumers parked at the end of
    // their streams, so the spawn block below would silently resume
    // from there and never re-derive history. Detect the wipe (empty
    // projection tables) and drop the stale durables first; the
    // projectors then recreate them with deliver-all. Must run before
    // any projector spawns. Failures are non-fatal — worst case is
    // the pre-#389 behaviour.
    if let Err(e) = projector::consumer_reset::reset_if_wiped(&jetstream, &pool).await {
        warn!(error = %e, "projector consumer reset check failed");
    }

    // v0.31 / #40: walk every registered inventory manifest and
    // CREATE TABLE IF NOT EXISTS for any `explode` specs. Idempotent
    // — re-running is a no-op. Done at startup (vs lazily in the
    // results projector) so cross-PC search queries can hit the
    // derived tables immediately, even before any new result lands.
    // CodeRabbit #85 fix: visibility on prewarm failures. Pre-fix
    // every failure branch (KV unreachable, keys() error, per-key
    // get() / deserialize) was silently dropped, so a busted
    // prewarm + a later search request would 500 with "no such
    // table" and zero startup log to explain why. Each branch
    // now logs at warn-level. The search path's
    // `ensure_table_cached` fallback (CR #3) covers the actual
    // table-creation gap, but logs help diagnose root cause.
    match jetstream
        .get_key_value(kanade_shared::kv::BUCKET_JOBS)
        .await
    {
        Ok(jobs_kv) => {
            let mut manifests = Vec::new();
            match jobs_kv.keys().await {
                Ok(keys_stream) => {
                    match futures::TryStreamExt::try_collect::<Vec<String>>(keys_stream).await {
                        Ok(keys) => {
                            for k in keys {
                                match jobs_kv.get(&k).await {
                                    Ok(Some(bytes)) => {
                                        match serde_json::from_slice::<
                                            kanade_shared::manifest::Manifest,
                                        >(&bytes)
                                        {
                                            Ok(m) => manifests.push(m),
                                            Err(e) => tracing::warn!(
                                                error = %e,
                                                job_key = %k,
                                                "explode prewarm: manifest deserialize failed",
                                            ),
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(e) => tracing::warn!(
                                        error = %e,
                                        job_key = %k,
                                        "explode prewarm: KV get failed",
                                    ),
                                }
                            }
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            "explode prewarm: collect keys failed",
                        ),
                    }
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "explode prewarm: keys() failed",
                ),
            }
            if let Err(e) = projector::explode::ensure_tables_for_jobs(&pool, manifests).await {
                error!(error = %e, "explode: startup table-ensure pass failed (will retry per-result)");
            }
        }
        Err(e) => tracing::warn!(
            error = %e,
            bucket = %kanade_shared::kv::BUCKET_JOBS,
            "explode prewarm: BUCKET_JOBS KV unreachable (ok if fresh install)",
        ),
    }

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
    // v0.40 Part 1: host-wide perf time-series projector. Same core-
    // NATS direct-subscribe shape as heartbeat (gaps acceptable, no
    // JetStream durability cost); writes to host_perf_samples
    // (append-only) instead of UPSERTing into agents.
    {
        let pool = pool.clone();
        let nats_client = nats.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::host_perf::run(nats_client, pool).await {
                error!(error = %e, "host_perf projector exited");
            }
        });
    }
    // v0.41 / Phase 2: per-process perf time-series projector. Only
    // sees traffic while an operator has opted a PC into investigation
    // mode (process_perf_enabled=true + expires_at in the future); on
    // a quiet fleet this projector wakes up and immediately blocks
    // back on the subscription with no DB writes.
    {
        let pool = pool.clone();
        let nats_client = nats.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::process_perf::run(nats_client, pool).await {
                error!(error = %e, "process_perf projector exited");
            }
        });
    }
    // v0.30 / PR α' unified: project agent `events.started.*.*` into
    // execution_results as in-flight rows. Pairs with results
    // projector — both UPSERT against execution_results.result_id
    // so the SPA Activity table sees one row per run that
    // transitions from running to finished.
    {
        let pool = pool.clone();
        let js = jetstream.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::events::run(js, pool).await {
                error!(error = %e, "events projector exited");
            }
        });
    }
    // Issue #246: per-PC observability timeline. Distinct from the
    // events.started projector above (lifecycle pairing) — this
    // one consumes the `obs.<pc_id>` stream into the dedicated
    // `obs_events` table that powers the SPA Timeline page.
    {
        let pool = pool.clone();
        let js = jetstream.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::obs_events::run(js, pool).await {
                error!(error = %e, "obs_events projector exited");
            }
        });
    }
    // v0.30 follow-up: periodic housekeeping that flips long-stale
    // `pending` executions to `expired`. Without this, fires whose
    // ExecResult never lands (offline targets, `run_as: user` with
    // no console session, agent died mid-script) pile up in the
    // Jobs page live chip indefinitely. 5 min cadence; the function
    // body details the policy.
    let _cleanup_handle = cleanup::spawn(pool.clone());

    // v0.35 / #88: explode-spec lookup cache. Built once, cloned
    // (cheap — Arc) into AppState for the search hot path and into
    // a watcher task that keeps it in sync with BUCKET_JOBS writes.
    // Prewarm walks every registered manifest at startup so the
    // first batch of search requests doesn't pay the cold-miss
    // latency.
    let explode_spec_cache = projector::spec_cache::ExplodeSpecCache::new();
    match projector::spec_cache::prewarm(&explode_spec_cache, &jetstream).await {
        Ok(n) => info!(cached = n, "explode spec cache prewarm done"),
        Err(e) => warn!(
            error = %e,
            "explode spec cache prewarm failed (watcher + miss-path fallback will recover)",
        ),
    }
    {
        let cache = explode_spec_cache.clone();
        let js = jetstream.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::spec_cache::run(cache, js).await {
                error!(error = %e, "explode spec cache watcher exited");
            }
        });
    }

    let app_state = api::AppState {
        pool: pool.clone(),
        nats,
        jetstream,
        explode_spec_cache,
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
        // RBAC middleware needs the SQLite pool to re-read the caller's
        // authoritative role / disabled flag on every request.
        .layer(axum::middleware::from_fn_with_state(
            pool.clone(),
            auth::verify,
        ))
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
