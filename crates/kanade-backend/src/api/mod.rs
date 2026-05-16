pub mod agent_config;
pub mod agent_groups;
pub mod agents;
pub mod audit;
pub mod deploy;
pub mod jetstream_status;
pub mod jobs;
pub mod results;
pub mod run;
pub mod schedules;
pub mod scripts;

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
        .route(
            "/api/agents/{pc_id}/effective_config",
            get(agent_config::effective),
        )
        .route(
            "/api/config",
            get(agent_config::get_global).put(agent_config::put_global),
        )
        .route(
            "/api/groups/{name}/config",
            get(agent_config::get_group)
                .put(agent_config::put_group)
                .delete(agent_config::delete_group),
        )
        .route(
            "/api/pcs/{pc_id}/config",
            get(agent_config::get_pc)
                .put(agent_config::put_pc)
                .delete(agent_config::delete_pc),
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
        .route("/api/run", post(run::run))
        .route("/api/agents/{pc_id}/ping", post(run::ping))
        .route("/api/scripts/{cmd_id}/revoke", post(scripts::revoke))
        .route("/api/scripts/{cmd_id}/unrevoke", post(scripts::unrevoke))
        .route("/api/jobs/{job_id}/kill", post(jobs::kill))
        .route("/api/jetstream/status", get(jetstream_status::status))
        .with_state(state)
        // Everything else (`/`, `/assets/...`, hash-router paths) is served
        // from the rust-embed bundle. The fallback runs after the API routes
        // above, so JSON endpoints take precedence.
        .fallback(crate::web::serve)
}

async fn health() -> &'static str {
    "ok"
}
