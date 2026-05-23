pub mod agent_config;
pub mod agent_groups;
pub mod agent_logs;
pub mod agent_releases;
pub mod agents;
pub mod audit;
pub mod exec;
pub mod executions;
pub mod health;
pub mod inventory;
pub mod jetstream_status;
pub mod jobs;
pub mod results;
pub mod run;
pub mod schedules;
pub mod schemas;
pub mod scripts;
pub mod yaml_body;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::routing::{delete, get, post};
use sqlx::SqlitePool;

/// 64 MB upper bound for `POST /api/agents/publish` multipart bodies.
/// kanade-agent.exe is ~13 MB on Windows; 64 MB leaves headroom for
/// debug builds and future on-disk growth without becoming a DoS vector.
const PUBLISH_BODY_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub nats: async_nats::Client,
    pub jetstream: async_nats::jetstream::Context,
    /// v0.35 / #88: explode-spec lookup cache, kept fresh by a KV
    /// `watch_all()` on BUCKET_JOBS. The /inventory/.../search/...
    /// hot path hits this instead of a NATS round-trip per request.
    /// `Clone` is cheap (Arc).
    pub explode_spec_cache: crate::projector::spec_cache::ExplodeSpecCache,
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
        // v0.29 / Issue #19: path param is now `result_id` (was
        // `request_id`); pre-v0.29 rows backfilled `result_id = request_id`
        // so existing browser-cached deep links still resolve.
        .route("/api/results/{result_id}", get(results::detail))
        .route("/api/executions", get(executions::list))
        .route("/api/executions/{exec_id}", get(executions::detail))
        .route("/api/audit", get(audit::list))
        .route("/api/exec/{job_id}", post(exec::create))
        .route(
            "/api/schedules",
            get(schedules::list).post(schedules::create),
        )
        .route("/api/schedules/{id}", delete(schedules::delete))
        .route("/api/schedules/{id}/disable", post(schedules::disable))
        .route("/api/run", post(run::run))
        .route("/api/agents/{pc_id}/ping", post(run::ping))
        .route("/api/scripts/status", get(scripts::list_status))
        .route("/api/scripts/{cmd_id}/revoke", post(scripts::revoke))
        .route("/api/scripts/{cmd_id}/unrevoke", post(scripts::unrevoke))
        .route("/api/jobs", get(jobs::list).post(jobs::create))
        .route("/api/jobs/{id}", delete(jobs::delete))
        .route("/api/jobs/{id}/yaml", get(jobs::get_yaml))
        .route("/api/jobs/{job_id}/kill", post(jobs::kill))
        .route("/api/schedules/{id}/yaml", get(schedules::get_yaml))
        .route("/api/schemas/manifest.json", get(schemas::manifest_schema))
        .route("/api/schemas/schedule.json", get(schemas::schedule_schema))
        .route("/api/jetstream/status", get(jetstream_status::status))
        .route("/api/health/fleet", get(health::fleet))
        // v0.37 / agent perf: per-job duration aggregates
        // (p50 / p95 / p99) over a recent window. Pure SQL over the
        // existing execution_results.{started,finished}_at — no
        // agent-side instrumentation needed.
        .route("/api/health/scan_durations", get(health::scan_durations))
        .route("/api/inventory/jobs", get(inventory::list_jobs))
        .route(
            "/api/inventory/by-job/{manifest_id}",
            get(inventory::list_for_job),
        )
        // v0.31 / #40: cross-PC search over a derived `explode`
        // table. `{field}` is the JSON array key, validated against
        // the manifest's explode spec.
        .route(
            "/api/inventory/{manifest_id}/search/{field}",
            get(inventory::search),
        )
        // v0.31 / #41: per-PC inventory history timeline.
        .route(
            "/api/inventory/{manifest_id}/history/pc/{pc_id}",
            get(inventory::history_for_pc),
        )
        // v0.35 / #90: fleet-wide history search across PCs. Same
        // response shape as /history/pc/{pc_id}; query string
        // carries optional `field`, `kind`, `since`, `until`,
        // `identity.<key>=<value>` filters plus `limit` / `offset`.
        // Each row's `pc_id` is what distinguishes it from the
        // per-PC variant.
        .route(
            "/api/inventory/{manifest_id}/history/search",
            get(inventory::fleet_history_search),
        )
        // v0.35 / #91: first_seen-per-PC aggregation. Returns one
        // row per matching PC with the earliest observed_at of any
        // matching event — operator buckets the result by date
        // client-side to draw the rollout-curve chart.
        .route(
            "/api/inventory/{manifest_id}/history/first_seen",
            get(inventory::first_seen),
        )
        .route("/api/inventory/{pc_id}", get(inventory::list_for_pc))
        .route("/api/agents/{pc_id}/logs", get(agent_logs::tail))
        .route("/api/agents/releases", get(agent_releases::list_releases))
        .route(
            "/api/agents/releases/{version}",
            delete(agent_releases::delete_release),
        )
        .route("/api/agents/rollout", post(agent_releases::rollout))
        .route(
            "/api/agents/publish",
            post(agent_releases::publish).layer(DefaultBodyLimit::max(PUBLISH_BODY_LIMIT)),
        )
        .with_state(state)
        // Everything else (`/`, `/assets/...`, hash-router paths) is served
        // from the rust-embed bundle. The fallback runs after the API routes
        // above, so JSON endpoints take precedence.
        .fallback(crate::web::serve)
}

async fn health() -> &'static str {
    "ok"
}
