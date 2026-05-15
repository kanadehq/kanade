pub mod agents;
pub mod deploy;
pub mod results;

use axum::Router;
use axum::extract::FromRef;
use axum::routing::{get, post};
use sqlx::SqlitePool;

/// State shared by every axum handler. Individual handlers extract just
/// the slices they need via [`FromRef`]; the result handler keeps using
/// `State<SqlitePool>` unmodified.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub nats: async_nats::Client,
    pub jetstream: async_nats::jetstream::Context,
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/agents", get(agents::list))
        .route("/api/agents/{pc_id}", get(agents::detail))
        .route("/api/results", get(results::list))
        .route("/api/results/{request_id}", get(results::detail))
        .route("/api/deploy", post(deploy::create))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
