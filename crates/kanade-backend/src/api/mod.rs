pub mod accounts;
pub mod agent_config;
pub mod agent_groups;
pub mod agent_logs;
pub mod agent_releases;
pub mod agents;
pub mod analytics;
pub mod app_packages;
pub mod audit;
pub mod checks;
pub mod collect;
pub mod exec;
pub mod executions;
pub mod fleet_perf;
pub mod freeze;
pub mod group_contacts;
pub mod health;
pub mod host_perf;
pub mod inventory;
pub mod jetstream_status;
pub mod jobs;
pub mod notifications;
pub mod obs_events;
pub mod password_setup;
pub mod process_perf;
pub mod results;
pub mod run;
pub mod schedules;
pub mod schemas;
pub mod script_objects;
pub mod scripts;
pub mod views;
pub mod yaml_body;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post, put};
use regex::Regex;
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
    /// Outbound SMTP relay, built from the `[mail]` config when present.
    /// `None` ⇒ email features are no-ops. `Arc` so `AppState`'s `Clone`
    /// (one per request) stays cheap and the relay's connection pool is
    /// shared. Used by the compliance-alert projector today; available to
    /// any future feature that needs to send mail.
    pub mailer: Option<std::sync::Arc<crate::mail::Mailer>>,
    /// Configured external base URL (`[server] public_url`) for absolute
    /// links in account emails. `None` ⇒ the link base is derived from the
    /// request `Host` header instead (see `password_setup::link_base`).
    pub public_url: Option<String>,
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
        // #770: PUBLIC one-time password setup/reset links + self-service
        // forgot-password (allow-listed in crate::auth::verify, like login).
        .route(
            "/api/auth/password-setup/{token}",
            get(password_setup::get_token).post(password_setup::set_password),
        )
        .route(
            "/api/auth/forgot-password",
            post(password_setup::forgot_password),
        )
        .route("/api/agents", get(agents::list))
        // Dashboard "version distribution" card — agent-version histogram
        // for the whole fleet. Static segment, so it resolves ahead of
        // the `{pc_id}` route below.
        .route("/api/agents/versions", get(agents::versions))
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
        // #720: generic obs_events rollups driven by the `aggregate:`
        // manifest hint, for the Analytics page (this superseded the
        // hardcoded `/api/utilization` rollup). Viewer-readable like the
        // other obs feeds.
        .route("/api/analytics", get(analytics::get))
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
        // Per-group notification email addresses (viewer-readable;
        // PUT is operator-gated below). Drives the Groups page email column.
        .route(
            "/api/groups/{name}/email",
            get(group_contacts::get_contacts),
        )
        .route(
            "/api/agents/{pc_id}/effective_config",
            get(agent_config::effective),
        )
        .route("/api/config", get(agent_config::get_global))
        // Built-in default EffectiveConfig — compiled-in floor values
        // the SPA global editor renders as per-field placeholders.
        // Read-only and viewer-open like the other config reads.
        .route("/api/config/defaults", get(agent_config::defaults))
        .route("/api/groups/{name}/config", get(agent_config::get_group))
        // Inherited (placeholder) EffectiveConfig for the SPA's per-scope
        // editors: what a field falls back to when left blank. Group =
        // built-in→global; PC = built-in→global→groups with the PC's own
        // scope excluded. Read-only, viewer-open like the config reads.
        .route(
            "/api/groups/{name}/config/inherited",
            get(agent_config::group_inherited),
        )
        .route("/api/pcs/{pc_id}/config", get(agent_config::get_pc))
        .route(
            "/api/pcs/{pc_id}/config/inherited",
            get(agent_config::pc_inherited),
        )
        .route("/api/results", get(results::list))
        // v0.29 / Issue #19: path param is now `result_id` (was
        // `request_id`); pre-v0.29 rows backfilled `result_id = request_id`
        // so existing browser-cached deep links still resolve.
        .route("/api/results/{result_id}", get(results::detail))
        // Live stdout/stderr tail for an in-flight job — the SPA's
        // "live" toggle polls this (job.tail.<pc_id> round-trip to the
        // agent; falls back to the persisted row once finished).
        .route("/api/results/{result_id}/tail", get(results::tail))
        .route("/api/executions", get(executions::list))
        .route("/api/executions/{exec_id}", get(executions::detail))
        .route("/api/audit", get(audit::list))
        .route("/api/schedules", get(schedules::list))
        // Dashboard "upcoming schedules" card — soonest fires across all
        // enabled calendar schedules. Static segment, ahead of `{id}/*`.
        .route("/api/schedules/upcoming", get(schedules::upcoming))
        .route("/api/freeze", get(freeze::get))
        .route("/api/scripts/status", get(scripts::list_status))
        .route("/api/jobs", get(jobs::list))
        .route("/api/jobs/{id}/yaml", get(jobs::get_yaml))
        // #743: standalone view resources (viewer-readable list + YAML).
        .route("/api/views", get(views::list))
        .route("/api/views/{id}/yaml", get(views::get_yaml))
        .route("/api/schedules/{id}/yaml", get(schedules::get_yaml))
        .route("/api/schedules/{id}/preview", get(schedules::preview))
        .route("/api/schedules/{id}/status", get(schedules::status))
        // #418 rollout coverage: per-schedule (full target roster vs
        // run state) + a batch summary for the list view. The literal
        // `/coverage` and the `{id}/coverage` param path don't collide
        // (different segment counts).
        .route("/api/schedules/coverage", get(schedules::coverage_summary))
        .route("/api/schedules/{id}/coverage", get(schedules::coverage))
        .route("/api/schemas/manifest.json", get(schemas::manifest_schema))
        .route("/api/schemas/schedule.json", get(schemas::schedule_schema))
        .route("/api/schemas/view.json", get(schemas::view_schema))
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
        // #574: cross-PC search over top-level scalar facts in
        // `facts_json` — no `explode` sub-table required. Columns are
        // the manifest's non-`table` `display` fields; the query
        // string carries the same Django-ish filter syntax as the
        // explode search plus `limit` / `offset`.
        .route(
            "/api/inventory/{manifest_id}/search-scalars",
            get(inventory::search_scalars),
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
        // #290 PR-E: fleet-wide compliance (operator-defined checks).
        .route("/api/checks", get(checks::list_all))
        // Phase E (KLP notifications): per-recipient confirmation
        // status for a sent notification. Read-only (viewer+); the
        // send side is operator-gated below.
        .route(
            "/api/notifications/{id}/ack_status",
            get(notifications::ack_status),
        )
        // Single sent notification (viewer+) — full content + acks for the
        // deep-linkable `/notifications/{id}` detail page. Kept adjacent to
        // the bare `/api/notifications` list route for readability (axum
        // routes by segment structure, not registration order, so the two
        // can't conflict regardless of which is declared first).
        .route("/api/notifications/{id}", get(notifications::detail))
        // Sent-notification history (viewer+) — replays the NOTIFICATIONS
        // stream so the SPA can show "what did I send" and deep-link each
        // entry into its ack_status view.
        .route("/api/notifications", get(notifications::list_sent))
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
        )
        // #219 collected bundles — list + download are viewer-readable
        // (the delete is operator-gated in the mutations router below).
        // The `{*key}` wildcard captures the full <pc_id>/<job_id>/<ts>.zip
        // key (it contains slashes, unlike the 2-segment package keys).
        .route("/api/collect/bundles", get(collect::list_bundles))
        .route("/api/collect/bundles/{*key}", get(collect::download_bundle));

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
            "/api/groups/{name}/email",
            put(group_contacts::put_contacts),
        )
        .route(
            "/api/pcs/{pc_id}/config",
            put(agent_config::put_pc).delete(agent_config::delete_pc),
        )
        .route("/api/exec/{job_id}", post(exec::create))
        // Phase E (KLP notifications): publish an end-user notification
        // (fans out to notifications.{all|group.X|pc.Y}).
        .route("/api/notifications", post(notifications::publish))
        // Completely recall (delete) a sent notification — deletes every
        // fan-out copy from the stream + broadcasts a live amend.
        .route(
            "/api/notifications/{id}/recall",
            post(notifications::recall),
        )
        // Edit a sent notification in place (content/expiry/priority/ack/toast;
        // audience immutable) — deletes the old copies + re-publishes merged.
        .route("/api/notifications/{id}", patch(notifications::edit))
        .route("/api/schedules", post(schedules::create))
        .route("/api/schedules/{id}", delete(schedules::delete))
        // #743: view create/delete (operator).
        .route("/api/views", post(views::create))
        .route("/api/views/{id}", delete(views::delete))
        .route("/api/schedules/{id}/disable", post(schedules::disable))
        .route("/api/schedules/{id}/enable", post(schedules::enable))
        .route("/api/freeze", put(freeze::set).delete(freeze::clear))
        .route("/api/run", post(run::run))
        .route("/api/agents/{pc_id}/ping", post(run::ping))
        .route("/api/scripts/{cmd_id}/revoke", post(scripts::revoke))
        .route("/api/scripts/{cmd_id}/unrevoke", post(scripts::unrevoke))
        .route("/api/jobs", post(jobs::create))
        .route("/api/jobs/{id}", delete(jobs::delete))
        // Clear orphaned compliance rows for a check (deleted / renamed
        // check job, decommissioned PC, a one-off test). Not cascaded
        // from job delete by design — see `checks::clear`.
        .route("/api/checks/{check_name}", delete(checks::clear))
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
        // #219: collected-bundle gc. No upload here — bundles are
        // produced by agents via the exec result path, not POSTed.
        .route(
            "/api/collect/bundles/{*key}",
            axum::routing::delete(collect::delete_bundle),
        )
        .route_layer(axum::middleware::from_fn(crate::auth::require_operator));

    // Account management — admin only.
    let admin = Router::new()
        .route("/api/accounts", get(accounts::list).post(accounts::create))
        .route(
            "/api/accounts/{username}",
            patch(accounts::update).delete(accounts::delete),
        )
        // #770: mail a one-time password setup/reset link to the account.
        .route(
            "/api/accounts/{username}/reset-link",
            post(accounts::reset_link),
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

/// Shared regex compiler for list-endpoint text filters (results /
/// audit / agents). `None` / empty → no filter; an invalid pattern →
/// 400 with the offending expression echoed back so the SPA can
/// surface it inline. Previously copy-pasted per handler; unified here
/// so the three pages behave identically.
pub(crate) fn compile(opt: Option<&str>) -> Result<Option<Regex>, (StatusCode, String)> {
    match opt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Regex::new(s)
            .map(Some)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid regex `{s}`: {e}"))),
        None => Ok(None),
    }
}
