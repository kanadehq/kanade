pub mod agents;
pub mod results;

use axum::Router;
use axum::routing::get;
use sqlx::SqlitePool;

pub fn router(pool: SqlitePool) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/agents", get(agents::list))
        .route("/api/agents/{pc_id}", get(agents::detail))
        .route("/api/results", get(results::list))
        .route("/api/results/{request_id}", get(results::detail))
        .with_state(pool)
}

async fn health() -> &'static str {
    "ok"
}
