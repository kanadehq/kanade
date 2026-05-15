mod api;
mod audit;
mod projector;
mod scheduler;
mod web;

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::Parser;
use kanade_shared::config::load_backend_config;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(
    name = "kanade-backend",
    about = "kanade backend (axum + SQLite projector)",
    version
)]
struct Cli {
    #[arg(long, default_value = "backend.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kanade_backend=debug,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = load_backend_config(&cli.config)
        .with_context(|| format!("load config from {:?}", cli.config))?;
    info!(
        bind = %cfg.server.bind,
        nats = %cfg.nats.url,
        db = %cfg.db.sqlite_path,
        "starting kanade-backend",
    );

    // SQLite open + migrate
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

    // NATS connect + JetStream context
    let nats = async_nats::connect(&cfg.nats.url)
        .await
        .with_context(|| format!("connect NATS at {}", cfg.nats.url))?;
    info!("connected to NATS");
    let jetstream = async_nats::jetstream::new(nats.clone());

    // Projectors run in the background; if either exits the backend keeps
    // serving HTTP (read-only API stays useful even if a stream is missing).
    {
        let pool = pool.clone();
        let js = jetstream.clone();
        tokio::spawn(async move {
            if let Err(e) = projector::inventory::run(js, pool).await {
                error!(error = %e, "inventory projector exited");
            }
        });
    }
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

    let app = api::router(app_state).layer(TraceLayer::new_for_http());

    let listener = TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("bind {}", cfg.server.bind))?;
    info!(bind = %cfg.server.bind, "axum serving");
    axum::serve(listener, app).await.context("axum serve")?;
    Ok(())
}
