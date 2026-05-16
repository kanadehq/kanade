pub mod agent_groups;
pub mod agents;
pub mod audit;
pub mod deploy;
pub mod results;
pub mod schedules;

use axum::Router;
use axum::extract::FromRef;
use axum::routing::{delete, get, post};
use sqlx::SqlitePool;

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
        .route(
            "/api/agents/{pc_id}/groups",
            get(agent_groups::list_groups)
                .put(agent_groups::set_groups)
                .post(agent_groups::add_group),
        )
        .route(
            "/api/agents/{pc_id}/groups/{group}",
            delete(agent_groups::remove_group),
        )
        .route("/api/results", get(results::list))
        .route("/api/results/{request_id}", get(results::detail))
        .route("/api/audit", get(audit::list))
        .route("/api/deploy", post(deploy::create))
        .route(
            "/api/schedules",
            get(schedules::list).post(schedules::create),
        )
        .route("/api/schedules/{id}", delete(schedules::delete))
        .with_state(state)
        // Everything else (`/`, `/assets/...`, hash-router paths) is served
        // from the rust-embed bundle. The fallback runs after the API routes
        // above, so JSON endpoints take precedence.
        .fallback(crate::web::serve)
}

async fn health() -> &'static str {
    "ok"
}
