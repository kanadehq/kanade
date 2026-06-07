pub mod accounts;
pub mod agent_config;
pub mod agent_groups;
pub mod agent_logs;
pub mod agent_releases;
pub mod agents;
pub mod app_packages;
pub mod audit;
pub mod exec;
pub mod executions;
pub mod fleet_perf;
pub mod health;
pub mod host_perf;
pub mod inventory;
pub mod jetstream_status;
pub mod jobs;
pub mod obs_events;
pub mod process_perf;
pub mod results;
pub mod run;
pub mod schedules;
pub mod schemas;
pub mod script_objects;
pub mod scripts;
pub mod yaml_body;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::routing::{delete, get, patch, post, put};
use sqlx::SqlitePool;

/// 64 MB upper bound for `POST /api/agents/publish` multipart bodies.
/// kanade-agent.exe is ~13 MB on Windows; 64 MB leaves headroom for
/// debug builds and future on-disk growth without becoming a DoS vector.
const PUBLISH_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// 8 GB upper bound for `POST /api/app-packages/{name}/{version}`
/// on 64-bit targets. Bigger than `PUBLISH_BODY_LIMIT` because app
/// packages cover third-party installers (Webex / Teams / Office
/// plug-ins, plus the occasional multi-GB SDK / VM image) whose
/// bundles can run from ~100 MB MSIs up to several-GB ISOs. The
/// handler streams the multipart field directly into
/// `ObjectStore::put` (chunked at ~128 KB per JetStream publish),
/// so this cap caps the *cap* — RSS stays flat regardless of
/// payload size.
///
/// Bump higher if a fleet ships > 8 GB single files; the JetStream
/// stream backing `OBJECT_APP_PACKAGES` has no per-message size
/// limit and the operator's only other constraint is
/// `max_file_store` in `configs/nats-server.conf` (50 GB default).
///
/// 32-bit fallback: `8 * 1024 * 1024 * 1024` overflows the `usize`
/// type on 32-bit targets. Backend builds we ship are all 64-bit
/// today, but `cargo check` on a 32-bit target would refuse to
/// compile without a guard — fall back to `usize::MAX` (= ~4 GB
/// minus a page) there. Gemini #284 MEDIUM.
#[cfg(target_pointer_width = "64")]
const APP_PACKAGE_BODY_LIMIT: usize = 8 * 1024 * 1024 * 1024;
#[cfg(not(target_pointer_width = "64"))]
const APP_PACKAGE_BODY_LIMIT: usize = usize::MAX;

/// 4 MB upper bound for `POST /api/script-objects/{name}/{version}`.
/// Manifest scripts are typically PowerShell / Bash bodies measured
/// in KB; 4 MB is generous enough to absorb embedded base64 helper
/// blobs without becoming a DoS lever the way the installer cap is.
/// If a future operator workflow needs to ship a script > 4 MB, the
/// right answer is almost always "split the binary helper into an
/// `app_packages` upload + a thin wrapper script that fetches it"
/// rather than relaxing this cap.
const SCRIPT_OBJECT_BODY_LIMIT: usize = 4 * 1024 * 1024;

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
    // RBAC is layered per group, not per handler:
    //   * `base`     — public health + login, plus read-only (`GET`)
    //                  and self-service routes that any authenticated
    //                  caller (viewer+) may hit. `/api/auth/login` is
    //                  allow-listed in `auth::verify`, so it is reachable
    //                  without a token.
    //   * `operator` — fleet mutations (exec / kill / config writes /
    //                  releases / object-store uploads). `route_layer`
    //                  with `auth::require_operator` rejects viewers 403.
    //   * `admin`    — account management. `route_layer` with
    //                  `auth::require_admin`.
    //
    // Merging combines same-path/different-method routers, so e.g.
    // `GET /api/config` (base) and `PUT /api/config` (operator) coexist
    // with the read open to viewers and the write gated to operators.
    let base = Router::new()
        .route("/health", get(health))
        // Public: backend build version (so the SPA can show it, even on
        // the login screen). Allow-listed in `crate::auth::verify`.
        .route("/api/version", get(version))
        // RBAC: credential login (public), self identity, self password.
        .route("/api/auth/login", post(accounts::login))
        .route("/api/auth/me", get(accounts::me))
        .route("/api/auth/change-password", post(accounts::change_password))
        .route("/api/agents", get(agents::list))
        .route("/api/agents/{pc_id}", get(agents::detail))
        // v0.40 Part 1: per-PC host-wide perf time-series. Bucketed
        // server-side via `?from=&to=&step=` so the SPA chart can
        // feed the response directly into Recharts without further
        // down-sampling.
        .route("/api/agents/{pc_id}/perf", get(host_perf::perf))
        // v0.41 / Phase 3: fleet-wide perf aggregates. Three sibling
        // endpoints driving the Dashboard cards — bucketed time-
        // series, top-N PC ranking, and a "currently investigating"
        // (process_perf-active) list.
        .route("/api/perf/fleet", get(fleet_perf::fleet))
        .route("/api/perf/top", get(fleet_perf::top))
        // Issue #246: per-PC observability timeline. `list` powers
        // the SPA Events page; `kinds` populates its filter chip;
        // `recent` is the dashboard "latest activity" feed.
        .route("/api/obs_events", get(obs_events::list))
        .route("/api/obs_events/kinds", get(obs_events::kinds))
        // Issue #391: distinct sources for the include/exclude chips.
        .route("/api/obs_events/sources", get(obs_events::sources))
        .route("/api/obs_events/recent", get(obs_events::recent))
        .route(
            "/api/perf/active-investigations",
            get(fleet_perf::active_investigations),
        )
        // v0.41 / Phase 2: latest top-N per-process snapshot for a
        // host that an operator has opted into investigation mode.
        // Empty `processes` array + null `latest_at` if process_perf
        // was never enabled for this PC (or its samples have aged
        // out of the 7-day retention).
        .route(
            "/api/agents/{pc_id}/processes",
            get(process_perf::processes),
        )
        // v0.42: stacked per-process time-series chart driver. Same
        // table as /processes, but bucketed in SQL with the window-
        // wide top-N names pinned for stable series colouring.
        // Anything outside the top-N collapses into one `other` series.
        .route(
            "/api/agents/{pc_id}/processes/timeline",
            get(process_perf::timeline),
        )
        .route("/api/agents/{pc_id}/groups", get(agent_groups::list_groups))
        // Group-centric inverse view — drives the SPA Groups page.
        .route("/api/groups", get(agent_groups::list_all_groups))
        .route(
            "/api/agents/{pc_id}/effective_config",
            get(agent_config::effective),
        )
        .route("/api/config", get(agent_config::get_global))
        .route("/api/groups/{name}/config", get(agent_config::get_group))
        .route("/api/pcs/{pc_id}/config", get(agent_config::get_pc))
        .route("/api/results", get(results::list))
        // v0.29 / Issue #19: path param is now `result_id` (was
        // `request_id`); pre-v0.29 rows backfilled `result_id = request_id`
        // so existing browser-cached deep links still resolve.
        .route("/api/results/{result_id}", get(results::detail))
        .route("/api/executions", get(executions::list))
        .route("/api/executions/{exec_id}", get(executions::detail))
        .route("/api/audit", get(audit::list))
        .route("/api/schedules", get(schedules::list))
        .route("/api/scripts/status", get(scripts::list_status))
        .route("/api/jobs", get(jobs::list))
        .route("/api/jobs/{id}/yaml", get(jobs::get_yaml))
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
        .route("/api/app-packages", get(app_packages::list_packages))
        .route(
            "/api/app-packages/{name}/{version}",
            get(app_packages::download),
        )
        .route("/api/script-objects", get(script_objects::list_objects))
        .route(
            "/api/script-objects/{name}/{version}",
            get(script_objects::download),
        );

    // Fleet mutations — operator+ only.
    let operator = Router::new()
        .route(
            "/api/agents/{pc_id}/groups",
            put(agent_groups::set_groups).post(agent_groups::add_group),
        )
        .route(
            "/api/agents/{pc_id}/groups/{group}",
            delete(agent_groups::remove_group),
        )
        .route("/api/config", put(agent_config::put_global))
        .route(
            "/api/groups/{name}/config",
            put(agent_config::put_group).delete(agent_config::delete_group),
        )
        .route(
            "/api/pcs/{pc_id}/config",
            put(agent_config::put_pc).delete(agent_config::delete_pc),
        )
        .route("/api/exec/{job_id}", post(exec::create))
        .route("/api/schedules", post(schedules::create))
        .route("/api/schedules/{id}", delete(schedules::delete))
        .route("/api/schedules/{id}/disable", post(schedules::disable))
        .route("/api/schedules/{id}/enable", post(schedules::enable))
        .route("/api/run", post(run::run))
        .route("/api/agents/{pc_id}/ping", post(run::ping))
        .route("/api/scripts/{cmd_id}/revoke", post(scripts::revoke))
        .route("/api/scripts/{cmd_id}/unrevoke", post(scripts::unrevoke))
        .route("/api/jobs", post(jobs::create))
        .route("/api/jobs/{id}", delete(jobs::delete))
        .route("/api/jobs/{job_id}/kill", post(jobs::kill))
        .route(
            "/api/agents/releases/{version}",
            delete(agent_releases::delete_release),
        )
        .route("/api/agents/rollout", post(agent_releases::rollout))
        .route(
            "/api/agents/publish",
            post(agent_releases::publish).layer(DefaultBodyLimit::max(PUBLISH_BODY_LIMIT)),
        )
        // Generic app-package distribution (kanade-client today;
        // third-party installers like Webex / Teams next). Distinct
        // from `agent_releases` so the lifecycles + audit channels
        // don't overlap — see `kanade-shared::kv::OBJECT_APP_PACKAGES`
        // for the rationale.
        .route(
            "/api/app-packages/{name}/{version}",
            post(app_packages::publish)
                .delete(app_packages::delete_package)
                .layer(DefaultBodyLimit::max(APP_PACKAGE_BODY_LIMIT)),
        )
        // Manifest-script Object Store (yukimemi/kanade#210). Sibling
        // of `app_packages`; distinct lifecycle (manifest-coupled vs
        // operator-curated installers) so the bucket + audit channels
        // are kept separate — see `kanade-shared::kv::OBJECT_SCRIPTS`.
        // Note: route prefix is `/api/script-objects` to avoid
        // collision with the existing `/api/scripts/...` revoke flow.
        .route(
            "/api/script-objects/{name}/{version}",
            post(script_objects::publish)
                .delete(script_objects::delete_object)
                .layer(DefaultBodyLimit::max(SCRIPT_OBJECT_BODY_LIMIT)),
        )
        .route_layer(axum::middleware::from_fn(crate::auth::require_operator));

    // Account management — admin only.
    let admin = Router::new()
        .route("/api/accounts", get(accounts::list).post(accounts::create))
        .route(
            "/api/accounts/{username}",
            patch(accounts::update).delete(accounts::delete),
        )
        .route_layer(axum::middleware::from_fn(crate::auth::require_admin));

    base.merge(operator)
        .merge(admin)
        .with_state(state)
        // Everything else (`/`, `/assets/...`, hash-router paths) is served
        // from the rust-embed bundle. The fallback runs after the API routes
        // above, so JSON endpoints take precedence.
        .fallback(crate::web::serve)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(serde::Serialize)]
struct VersionResponse {
    version: &'static str,
}

/// `GET /api/version` — the backend binary's build version. Public (no
/// auth) so the SPA can render it in the sidebar before/after login.
async fn version() -> axum::Json<VersionResponse> {
    axum::Json(VersionResponse {
        version: env!("CARGO_PKG_VERSION"),
    })
}
