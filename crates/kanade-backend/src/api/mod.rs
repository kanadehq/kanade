pub mod accounts;
pub mod agent_config;
pub mod agent_groups;
pub mod agent_installer;
pub mod agent_logs;
pub mod agent_meta;
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
pub mod group_defs;
pub mod group_sql;
pub mod health;
pub mod host_perf;
pub mod inventory;
pub mod jetstream_status;
pub mod jobs;
pub mod notifications;
pub mod obs_events;
pub mod password_setup;
pub mod permission_groups;
pub mod process_perf;
pub mod query;
pub mod remote;
pub mod results;
pub mod run;
pub mod schedules;
pub mod schemas;
pub mod script_objects;
pub mod scripts;
pub mod server;
pub mod server_settings;
pub mod sql_like;
pub mod time_bounds;
pub mod view_sql;
pub mod views;
pub mod yaml_body;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post, put};
use kanade_shared::feature::Feature;
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
    /// Dedicated **read-only** pool over the same SQLite file, opened with
    /// `SQLITE_OPEN_READONLY`. Only the admin ad-hoc `POST /api/query`
    /// endpoint uses it, so a stray write in operator SQL fails at the
    /// SQLite layer rather than by convention. See `api::query`.
    pub query_pool: SqlitePool,
    pub nats: async_nats::Client,
    /// #1165 stage 2: the only route a `Command` takes to the wire, so no
    /// call site can publish one without its provenance signature. Deliberately
    /// not reachable through [`AppState::nats`] — that handle stays for every
    /// other plane (results, kill, audit, notifications), none of which a
    /// signature covers. `Arc` keeps `AppState`'s per-request `Clone` cheap.
    pub commands: std::sync::Arc<crate::command_publisher::CommandPublisher>,
    pub jetstream: async_nats::jetstream::Context,
    /// v0.35 / #88: explode-spec lookup cache, kept fresh by a KV
    /// `watch_all()` on BUCKET_JOBS. The /inventory/.../search/...
    /// hot path hits this instead of a NATS round-trip per request.
    /// `Clone` is cheap (Arc).
    pub explode_spec_cache: crate::projector::spec_cache::ExplodeSpecCache,
    /// #vuln-roadmap PR3: in-memory materialization cache for SQL-backed
    /// `view:` widgets, keyed per `(view_id, widget index)`. A derived cache
    /// (recomputed from the read-only query on the widget's `refresh`
    /// cadence), so it needs no durability — `Arc` keeps `AppState`'s `Clone`
    /// cheap and shares the map across requests. See `api::view_sql`.
    pub sql_view_cache: view_sql::SqlViewCache,
    /// #1032: in-memory membership cache for dynamic (SQL) `group:` defs, keyed
    /// by group id. Same derived-cache shape as `sql_view_cache` — recomputed
    /// from the read-only query on the group's `refresh` cadence, so it needs
    /// no durability and `Arc` keeps `AppState`'s `Clone` cheap. Shared with
    /// the scheduler (`resolve_roster`). See `api::group_sql`.
    pub group_cache: group_sql::GroupCache,
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
    /// This backend's own configured `[nats] url`. `GET
    /// /api/agents/installer` writes it into the bundled `agent.toml` when
    /// the operator hasn't configured `agent_install.nats_url` in server
    /// settings — a fresh agent should dial the same broker the backend
    /// does by default.
    pub nats_url: String,
    /// #1191: in-memory rate limiting + lockout for the public login route.
    pub login_throttle: std::sync::Arc<crate::login_throttle::LoginThrottle>,
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
        // #1260: this backend's own signing identity, so a fleet-wide "does
        // everyone trust the key we actually sign with" is a comparison rather
        // than an assumption. Authed (not allow-listed in `auth::verify`).
        .route("/api/command-signing", get(command_signing))
        // RBAC: credential login (public), self identity, self password.
        .route("/api/auth/login", post(accounts::login))
        .route("/api/auth/me", get(accounts::me))
        .route("/api/auth/change-password", post(accounts::change_password))
        // #1192: self-service TOTP MFA enrolment (authed, like change-password).
        .route("/api/auth/mfa/init", post(accounts::mfa_init))
        .route("/api/auth/mfa/verify", post(accounts::mfa_verify))
        .route("/api/auth/mfa/disable", post(accounts::mfa_disable))
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
        // #1140: the operator's end of the remote-assistance relay. Sits in
        // `base` rather than the `operator` group on purpose — this route is
        // allow-listed out of `auth::verify` (its credential rides
        // `Sec-WebSocket-Protocol`, which the middleware cannot read), so no
        // `Claims` reach the `route_layer` gates and they would silently pass
        // anyone. The handler does identity, role and feature itself; putting
        // it under `operator` would look like a gate while enforcing nothing.
        .route("/api/remote/{pc_id}/ws", get(remote::ws))
        .route("/api/agents", get(agents::list))
        // Dashboard "version distribution" card — agent-version histogram
        // for the whole fleet. Static segment, so it resolves ahead of
        // the `{pc_id}` route below.
        .route("/api/agents/versions", get(agents::versions))
        // #1051: distinct agent_meta keys for the Agents column picker +
        // metadata search dropdown. Static segment, resolves ahead of the
        // `{pc_id}` route below (same as `/versions`).
        .route("/api/agents/meta-keys", get(agents::meta_keys))
        // #1357: agent_meta for the rows a page is currently showing, so a
        // table that isn't the Agents list can offer them as columns.
        // Static segment, so it resolves ahead of the `{pc_id}` routes.
        .route("/api/agents/meta", get(agents::meta_bulk))
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
        .route("/api/obs_events/lane_seeds", get(obs_events::lane_seeds))
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
        // Per-PC operator key/value metadata (viewer-readable; PUT is
        // operator-gated below). Drives the agent detail attributes card.
        .route("/api/agents/{pc_id}/meta", get(agent_meta::get_meta))
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
        // Operator-editable backend server settings (viewer-readable; PUT
        // is operator-gated below). Drives the Settings page "server
        // settings" tab. The `/defaults` sibling returns the compiled-in
        // floor the SPA renders as faint placeholders (mirrors
        // /api/config/defaults); different segment counts, so it can't
        // collide with the bare route.
        .route("/api/server-settings", get(server_settings::get))
        .route(
            "/api/server-settings/defaults",
            get(server_settings::defaults),
        )
        .route("/api/scripts/status", get(scripts::list_status))
        .route("/api/jobs", get(jobs::list))
        .route("/api/jobs/{id}/yaml", get(jobs::get_yaml))
        // #743: standalone view resources (viewer-readable list + YAML).
        .route("/api/views", get(views::list))
        .route("/api/views/{id}/yaml", get(views::get_yaml))
        // #1032: group-definition resources (viewer-readable list + YAML +
        // resolved-members preview).
        .route("/api/group-defs", get(group_defs::list))
        .route("/api/group-defs/{id}/yaml", get(group_defs::get_yaml))
        .route("/api/group-defs/{id}/members", get(group_defs::members))
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
        .route(
            "/api/schemas/group-def.json",
            get(schemas::group_def_schema),
        )
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
        // Self-service installer download (viewer+ base router; the real
        // restriction is the `agent-install` page feature, so a restricted
        // "download user" can fetch it without holding Rollout). GET, no
        // body: the ZIP always bundles the latest release, and the NATS
        // coordinates come from server settings, not the caller.
        .route("/api/agents/installer", get(agent_installer::installer))
        // One-liner installer scripts (same gate). Each embeds the
        // caller's own Bearer token so the script's inner archive
        // download authenticates as the same account.
        .route(
            "/api/agents/installer.ps1",
            get(agent_installer::installer_ps1),
        )
        .route(
            "/api/agents/installer.sh",
            get(agent_installer::installer_sh),
        )
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
        // Manually drop an agent from the registry (GET detail lives on
        // the viewer router; same path, different method, like the
        // /groups routes below). A live agent re-registers on its next
        // heartbeat — see agents::delete.
        .route("/api/agents/{pc_id}", delete(agents::delete))
        .route(
            "/api/agents/{pc_id}/groups",
            put(agent_groups::set_groups).post(agent_groups::add_group),
        )
        .route(
            "/api/agents/{pc_id}/groups/{group}",
            delete(agent_groups::remove_group),
        )
        // Replace a PC's operator key/value metadata (operator+). GET
        // lives on the viewer router above; same path, different method,
        // like the /groups routes.
        .route("/api/agents/{pc_id}/meta", put(agent_meta::put_meta))
        .route("/api/config", put(agent_config::put_global))
        .route(
            "/api/groups/{name}/config",
            put(agent_config::put_group).delete(agent_config::delete_group),
        )
        .route(
            "/api/groups/{name}/email",
            put(group_contacts::put_contacts),
        )
        // Replace the backend server-settings document (operator+).
        .route("/api/server-settings", put(server_settings::put))
        // Helpdesk unlock codes (operator+). Deliberately NOT part of the
        // document PUT above: responses blank the stored hash, so a form
        // round-trip must never be able to write the field back.
        .route(
            "/api/server-settings/support-codes/{scope}",
            put(server_settings::put_support_code).delete(server_settings::delete_support_code),
        )
        // Restart the backend service (operator+). The backend exits
        // non-zero and the SCM's failure-recovery actions relaunch it —
        // used to apply a server_settings change (e.g. SMTP) that's only
        // read at startup (#962).
        .route("/api/server/restart", post(server::restart))
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
        // #1032: group-definition create/delete (operator).
        .route("/api/group-defs", post(group_defs::create))
        .route("/api/group-defs/{id}", delete(group_defs::delete))
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
        // #1008 Phase 3: named permission groups (reusable page allow-lists
        // shared by many accounts). Admin-only CRUD.
        .route(
            "/api/permission-groups",
            get(permission_groups::list).post(permission_groups::create),
        )
        .route(
            "/api/permission-groups/{name}",
            patch(permission_groups::update).delete(permission_groups::delete),
        )
        // Ad-hoc read-only SQL over the projector DB. Admin-only: raw SQL
        // can read every projected table (emails, audit, …), so it sits in
        // the highest tier even though it only ever reads. The handler
        // runs on the read-only `query_pool`. See `api::query`.
        .route("/api/query", post(query::execute))
        .route_layer(axum::middleware::from_fn(crate::auth::require_admin));

    base.merge(operator)
        .merge(admin)
        .with_state(state)
        // Per-account PAGE enforcement (horizontal axis), layered over the
        // whole API. Runs after `auth::verify` (added in `main`, so it has
        // injected `Claims`) and after routing (so `MatchedPath` is set,
        // which `feature_for_path` keys off). Unrestricted callers pass
        // straight through; restricted ones get only their allow-listed
        // features plus a small infrastructure set — see
        // `auth::require_features`.
        .layer(axum::middleware::from_fn(crate::auth::require_features))
        // Everything else (`/`, `/assets/...`, hash-router paths) is served
        // from the rust-embed bundle. The fallback runs after the API routes
        // above, so JSON endpoints take precedence.
        .fallback(crate::web::serve)
}

/// Map a matched route pattern (`MatchedPath`, e.g. `/api/agents/{pc_id}`)
/// to the page **feature** that owns it, for per-account page enforcement
/// (`auth::require_features`). `None` = **commons**: a route open to any
/// *unrestricted* caller — the public / self-service routes, the Dashboard
/// landing feeds, and the shared fleet substrate. For a *restricted*
/// account (a non-NULL allow-list) commons routes are closed except the
/// small infrastructure allow-list in `auth::RESTRICTED_COMMONS`.
///
/// This is the single, auditable route→feature table. Commons-by-default is
/// deliberate (most endpoints are shared substrate); a NEW page-specific
/// endpoint must be added here to become gated.
///
/// Note: gating is by path, not method. A page's read and its mutation share
/// a `MatchedPath`, so both are gated together (the vertical role gate still
/// separately blocks a viewer's write). Endpoints the Dashboard also consumes
/// are intentionally left commons so restricting a page never blanks the
/// always-visible home.
pub fn feature_for_path(path: &str) -> Option<Feature> {
    Some(match path {
        // --- Inventory ---
        "/api/inventory/jobs"
        | "/api/inventory/by-job/{manifest_id}"
        | "/api/inventory/{manifest_id}/search/{field}"
        | "/api/inventory/{manifest_id}/search-scalars"
        | "/api/inventory/{manifest_id}/history/pc/{pc_id}"
        | "/api/inventory/{manifest_id}/history/search"
        | "/api/inventory/{manifest_id}/history/first_seen"
        | "/api/inventory/{pc_id}" => Feature::Inventory,

        // --- Compliance ---
        "/api/checks" | "/api/checks/{check_name}" => Feature::Compliance,

        // --- Analytics ---
        "/api/analytics" => Feature::Analytics,

        // --- Activity (executions + results) ---
        "/api/results"
        | "/api/results/{result_id}"
        | "/api/results/{result_id}/tail"
        | "/api/executions"
        | "/api/executions/{exec_id}" => Feature::Activity,

        // --- Events (obs_events; `recent` stays commons for the dashboard) ---
        "/api/obs_events"
        | "/api/obs_events/kinds"
        | "/api/obs_events/lane_seeds"
        | "/api/obs_events/sources" => Feature::Events,

        // --- Audit ---
        "/api/audit" => Feature::Audit,

        // --- Remote assistance (#1140) ---
        //
        // Listed for completeness, NOT for enforcement: this route is
        // allow-listed out of `auth::verify`, so `require_features` never
        // sees `Claims` for it and `api::remote::ws` checks the feature
        // itself. Omitting it here would be worse than redundant — this
        // table documents itself as the single auditable route→feature map,
        // and an absent route reads as "commons, open to any authenticated
        // caller", which a live view of someone's desktop is not.
        "/api/remote/{pc_id}/ws" => Feature::Remote,

        // --- Logs (per-PC log tail page) ---
        "/api/agents/{pc_id}/logs" => Feature::Logs,

        // --- Collect ---
        "/api/collect/bundles" | "/api/collect/bundles/{*key}" => Feature::Collect,

        // --- Jobs (incl. the script-command revoke lifecycle, which the
        //     SPA surfaces on the Jobs page — Jobs.tsx) ---
        "/api/jobs"
        | "/api/jobs/{id}/yaml"
        | "/api/jobs/{id}"
        | "/api/jobs/{job_id}/kill"
        | "/api/scripts/status"
        | "/api/scripts/{cmd_id}/revoke"
        | "/api/scripts/{cmd_id}/unrevoke" => Feature::Jobs,

        // --- Schedules (upcoming + coverage summary stay commons: dashboard) ---
        "/api/schedules"
        | "/api/schedules/{id}/yaml"
        | "/api/schedules/{id}/preview"
        | "/api/schedules/{id}/status"
        | "/api/schedules/{id}/coverage"
        | "/api/schedules/{id}"
        | "/api/schedules/{id}/disable"
        | "/api/schedules/{id}/enable" => Feature::Schedules,

        // --- Views ---
        "/api/views" | "/api/views/{id}/yaml" | "/api/views/{id}" => Feature::Views,

        // --- Group definitions (#1032) — the declarative/dynamic `groups/`
        // manifest kind, and since #1274 the only thing the SPA's single
        // /groups page lists. Gated by the same `groups` visibility feature
        // as the membership + contacts endpoints below. ---
        "/api/group-defs"
        | "/api/group-defs/{id}/yaml"
        | "/api/group-defs/{id}/members"
        | "/api/group-defs/{id}" => Feature::Groups,

        // --- Notifications ---
        "/api/notifications"
        | "/api/notifications/{id}"
        | "/api/notifications/{id}/ack_status"
        | "/api/notifications/{id}/recall" => Feature::Notifications,

        // --- Rollout (agent releases + rollout) ---
        "/api/agents/releases"
        | "/api/agents/releases/{version}"
        | "/api/agents/rollout"
        | "/api/agents/publish" => Feature::Rollout,

        // --- Agent install (self-service installer download) ---
        //
        // Its own feature, NOT Rollout: the whole point is a restricted
        // "download user" (viewer + only this feature) that can fetch the
        // installer ZIP without also holding release publish/rollout.
        "/api/agents/installer" | "/api/agents/installer.ps1" | "/api/agents/installer.sh" => {
            Feature::AgentInstall
        }

        // --- Apps (app packages + script objects) ---
        "/api/app-packages"
        | "/api/app-packages/{name}/{version}"
        | "/api/script-objects"
        | "/api/script-objects/{name}/{version}" => Feature::Apps,

        // --- Groups (the Groups page; per-agent membership stays commons) ---
        "/api/groups" | "/api/groups/{name}/email" => Feature::Groups,

        // --- Config (agent-config editor; `inherited`/`effective`/`defaults`
        //     stay commons as they back per-PC detail views too) ---
        "/api/config" | "/api/groups/{name}/config" | "/api/pcs/{pc_id}/config" => Feature::Config,

        // --- JetStream ---
        "/api/jetstream/status" => Feature::Jetstream,

        // --- Run ---
        "/api/run" => Feature::Run,

        // --- Exec ---
        "/api/exec/{job_id}" => Feature::Exec,

        // --- Settings (server settings + support codes + backend restart) ---
        //
        // The support-code routes belong here with the settings document
        // they live inside: without an arm they would fall through to
        // commons, so an operator whose permission group denies the
        // Settings page could still mint or rotate the codes that unlock
        // helpdesk-only jobs across the fleet — a wider capability than the
        // generic settings PUT sitting next to them.
        "/api/server-settings"
        | "/api/server-settings/support-codes/{scope}"
        | "/api/server/restart" => Feature::Settings,

        // --- Accounts (also admin-gated; raw SQL + permission groups here) ---
        "/api/accounts"
        | "/api/accounts/{username}"
        | "/api/accounts/{username}/reset-link"
        | "/api/permission-groups"
        | "/api/permission-groups/{name}"
        | "/api/query" => Feature::Accounts,

        // Commons: everything else (health, version, auth/*, the fleet
        // roster + per-PC detail/perf, dashboard feeds, editor schemas,
        // freeze banner, `*/defaults`, `*/inherited`, ...).
        _ => return None,
    })
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

/// #1260: which command-signing key this backend actually signs with.
///
/// Both fields are `null` when it is not signing. That is a real, expected
/// state — during stage 1, and after a key is removed — and a caller must be
/// able to tell it from a failed request: one means "nothing to compare
/// against", the other means "ask again". Encoding it as an error would make a
/// transient hiccup look like a fleet-wide key mismatch.
#[derive(serde::Serialize)]
struct CommandSigningResponse {
    kid: Option<String>,
    fingerprint: Option<String>,
}

/// `GET /api/command-signing` — the public half of this backend's signing key.
///
/// Exists because #1229 gave every agent a `kid:fingerprint` to report but
/// left nothing to compare it *against*. A ring carrying the right `kid` and
/// the wrong bytes refuses every command once enforcement is on and never
/// self-heals — the reload path fires on an *unknown* key, and this one is
/// known — so until the expected value is queryable, that host is
/// indistinguishable from a healthy one.
///
/// Authenticated (viewer+) rather than public like `/api/version`. A public
/// key fingerprint is not a secret, but it describes fleet configuration and
/// there is no reason for it to be readable before login.
async fn command_signing(State(st): State<AppState>) -> axum::Json<CommandSigningResponse> {
    let (kid, fingerprint) = match st.commands.identity_parts() {
        Some((kid, fp)) => (Some(kid.to_string()), Some(fp)),
        None => (None, None),
    };
    axum::Json(CommandSigningResponse { kid, fingerprint })
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

#[cfg(test)]
mod feature_map_tests {
    use super::*;

    #[test]
    fn gated_routes_map_to_their_feature() {
        assert_eq!(
            feature_for_path("/api/inventory/jobs"),
            Some(Feature::Inventory)
        );
        assert_eq!(feature_for_path("/api/checks"), Some(Feature::Compliance));
        assert_eq!(
            feature_for_path("/api/results/{result_id}"),
            Some(Feature::Activity)
        );
        assert_eq!(feature_for_path("/api/audit"), Some(Feature::Audit));
        assert_eq!(
            feature_for_path("/api/collect/bundles/{*key}"),
            Some(Feature::Collect)
        );
        assert_eq!(
            feature_for_path("/api/jetstream/status"),
            Some(Feature::Jetstream)
        );
        assert_eq!(
            feature_for_path("/api/server-settings"),
            Some(Feature::Settings)
        );
        // The support-code routes gate with the same page. Falling through
        // to commons would let a Settings-denied operator mint the codes
        // that unlock helpdesk-only jobs (Claude review on #1166).
        assert_eq!(
            feature_for_path("/api/server-settings/support-codes/{scope}"),
            Some(Feature::Settings)
        );
        assert_eq!(feature_for_path("/api/query"), Some(Feature::Accounts));
        // The installer download gates on its OWN feature — the whole point
        // of the split is a "download user" holding agent-install WITHOUT
        // Rollout (release publish / rollout stay operator territory). The
        // one-liner script endpoints gate with it.
        assert_eq!(
            feature_for_path("/api/agents/installer"),
            Some(Feature::AgentInstall)
        );
        assert_eq!(
            feature_for_path("/api/agents/installer.ps1"),
            Some(Feature::AgentInstall)
        );
        assert_eq!(
            feature_for_path("/api/agents/installer.sh"),
            Some(Feature::AgentInstall)
        );
        // #1032: group-def routes gate with the Groups page (its SPA
        // management page lives alongside the membership page).
        assert_eq!(feature_for_path("/api/group-defs"), Some(Feature::Groups));
        assert_eq!(
            feature_for_path("/api/group-defs/{id}/members"),
            Some(Feature::Groups)
        );
        // The script-command revoke lifecycle lives on the Jobs page, so it
        // gates under Jobs (not Run) — otherwise a Jobs-restricted operator
        // could revoke/unrevoke scripts (Gemini HIGH on #1009).
        assert_eq!(feature_for_path("/api/scripts/status"), Some(Feature::Jobs));
        assert_eq!(
            feature_for_path("/api/scripts/{cmd_id}/revoke"),
            Some(Feature::Jobs)
        );
        assert_eq!(
            feature_for_path("/api/scripts/{cmd_id}/unrevoke"),
            Some(Feature::Jobs)
        );
    }

    #[test]
    fn commons_routes_are_ungated() {
        // Public / self-service.
        assert_eq!(feature_for_path("/api/version"), None);
        assert_eq!(feature_for_path("/api/auth/me"), None);
        // Shared fleet substrate + dashboard feeds stay open so a page
        // restriction never blanks the always-visible home.
        assert_eq!(feature_for_path("/api/agents"), None);
        assert_eq!(feature_for_path("/api/agents/{pc_id}"), None);
        assert_eq!(feature_for_path("/api/perf/fleet"), None);
        assert_eq!(feature_for_path("/api/obs_events/recent"), None);
        assert_eq!(feature_for_path("/api/schedules/upcoming"), None);
        assert_eq!(feature_for_path("/api/config/defaults"), None);
        // An unknown / future path is commons by default.
        assert_eq!(feature_for_path("/api/something-new"), None);
    }

    #[test]
    fn read_and_mutation_share_the_gate() {
        // `GET` and `PUT` on `/api/config` share a MatchedPath, so both are
        // gated under Config (the vertical role gate handles read-vs-write).
        assert_eq!(feature_for_path("/api/config"), Some(Feature::Config));
    }
}
