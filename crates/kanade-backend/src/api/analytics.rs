//! `GET /api/analytics` — generic `obs_events` rollups driven by the
//! `aggregate:` manifest hint (#720). Discovers every job's
//! [`AggregateWidget`] specs from `BUCKET_JOBS` and computes each into a
//! render-ready payload, so an operator can chart any emitted event from
//! YAML without a Rust change. This replaced the old hardcoded
//! presence/app_sample/web_visit rollup (`api/utilization.rs`) once the
//! example configs reached parity.
//!
//! `?from=&to=` are RFC3339 UTC bounds (from inclusive, to exclusive;
//! both omitted ⇒ last 24h). `?tz_offset_minutes=` buckets the hourly
//! timeline into the operator's local hours. `?pc_id=` selects the scope:
//! present ⇒ per-PC (`scope: pc`) widgets for that PC; absent ⇒
//! fleet-wide (`scope: fleet`) widgets across all PCs.
//!
//! SQL is static per shape (the lint forbids dynamic assembly); JSON
//! paths are bound into `json_extract(payload, '$.' || ?)` and were
//! charset-validated at create time, and the source/pc filters are
//! `(? IS NULL OR col = ?)` gates so one statement serves both scopes.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use kanade_shared::kv::{BUCKET_JOBS, BUCKET_VIEWS};
use kanade_shared::manifest::{
    AggregateAgg, AggregateRender, AggregateScope, AggregateTimeBucket, AggregateTransform,
    AggregateWidget, Manifest, SqlWidget, View,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::{debug, warn};

use super::AppState;
use super::time_bounds::bounds_in_range;
use super::view_sql;

/// Default top-N for grouped (`bar`) widgets when the spec omits `limit`.
const DEFAULT_LIMIT: i64 = 10;

/// Cap on rows pulled for a Rust-side `transform: host` fold — a busy day
/// is well under this; the bound just stops a pathological PC from pulling
/// an unbounded set into memory (mirrors the old utilization top-sites).
const MAX_RAW_ROWS: i64 = 10_000;

#[derive(Deserialize)]
pub struct AnalyticsQuery {
    /// RFC3339 lower bound (inclusive). Default: `to` − 24h.
    pub from: Option<DateTime<Utc>>,
    /// RFC3339 upper bound (exclusive). Default: now.
    pub to: Option<DateTime<Utc>>,
    /// Minutes to ADD to a UTC `at` for local hour-of-day bucketing
    /// (e.g. JST = 540). Default 0. Clamped to ±900.
    pub tz_offset_minutes: Option<i64>,
    /// When set, compute the `scope: pc` widgets for this PC. When
    /// omitted, compute the `scope: fleet` widgets across all PCs.
    pub pc_id: Option<String>,
    /// When `true`, return only widgets flagged `pin_dashboard: true` — the
    /// subset the main Dashboard promotes. Absent / `false` ⇒ every widget
    /// (the Analytics page's behaviour).
    pub pinned: Option<bool>,
}

/// The shared query context for one request — the pieces every widget
/// query needs. Bundled so the per-shape helpers stay readable (and under
/// the argument-count lint).
struct Ctx<'a> {
    pool: &'a SqlitePool,
    /// `Some` ⇒ filter to one PC (per-PC scope); `None` ⇒ all PCs (fleet).
    /// Bound into the `(? IS NULL OR pc_id = ?)` gate either way.
    pc_id: Option<&'a str>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    /// strftime modifier shifting `at` into local time (e.g. `+540 minutes`).
    tz_mod: &'a str,
}

#[derive(Serialize, Clone, Debug)]
pub struct BarRow {
    pub label: String,
    pub value: i64,
    /// `value × sample_minutes` when the widget declares a cadence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_minutes: Option<i64>,
}

/// One local hour-of-day bucket for a `timeline` widget.
#[derive(Serialize, Clone, Debug)]
pub struct HourBucket {
    pub hour: i64,
    pub total: i64,
    pub active: i64,
}

/// One raw operational event for an `op_timeline` widget. The SPA folds a
/// window's worth of these into lane spans (power/session/sleep), so the
/// span-reconstruction logic lives in one place (shared with the Events
/// page strip) rather than being duplicated in Rust.
#[derive(Serialize, Clone, Debug)]
pub struct OpEvent {
    pub at: DateTime<Utc>,
    pub kind: String,
}

/// The render-specific payload. Tagged by `render` so the SPA picks the
/// matching widget component; flattened into [`WidgetResult`].
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "render", rename_all = "lowercase")]
pub enum WidgetData {
    /// #vuln-roadmap PR3: the full result grid of a SQL-backed `view:` widget.
    /// `columns` is the (optionally relabelled) header; each `rows` entry is a
    /// row of JSON cells aligned to it. New renderer on the SPA.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
    },
    /// #vuln-roadmap PR3: parts-of-a-whole for a SQL-backed `view:` widget.
    /// Reuses [`BarRow`] (label + value); `donut` asks the SPA for a hole with
    /// the total in the centre. New renderer on the SPA (recharts).
    Pie {
        rows: Vec<BarRow>,
        donut: bool,
    },
    Bar {
        rows: Vec<BarRow>,
    },
    Gauge {
        total: i64,
        active: i64,
        ratio: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        est_minutes: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        first: Option<DateTime<Utc>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        last: Option<DateTime<Utc>>,
    },
    Timeline {
        /// `"ratio"` when the widget declares a `bool_path` (per-hour
        /// active/total proportion — a presence strip); `"count"` for
        /// a pure volume timeline (e.g. boot / logon counts, where
        /// `active = total`). The SPA needs this to render correctly:
        /// a count strip must scale each bar's HEIGHT by its hour's
        /// magnitude and label it "count", whereas the ratio strip
        /// fills each populated hour by its active proportion and
        /// labels it "active / idle". Without the discriminator the
        /// SPA filled every populated hour to full height and called
        /// it "active / idle" — flattening all magnitude (every busy
        /// hour looked identical, the source of the "different days,
        /// same max" confusion) and mislabelling volume as presence.
        metric: &'static str,
        buckets: Vec<HourBucket>,
    },
    Stat {
        value: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        est_minutes: Option<i64>,
    },
    /// A per-PC operational swimlane. Carries the query window plus the raw
    /// operational events in it; the SPA reconstructs the power/session/sleep
    /// lane spans (clamping open intervals to `[from, to]`). Serialized with
    /// `render: "op_timeline"`.
    #[serde(rename = "op_timeline")]
    OpTimeline {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        events: Vec<OpEvent>,
        /// The agent's `agents.last_heartbeat` (None when the PC has no
        /// agents row yet). The SPA uses it to gate the live edge of the
        /// lanes: state *after* the last heartbeat is unconfirmed — the
        /// agent is offline / unreachable, and an unexpected power loss
        /// emits nothing — so the strip renders that tail as uncertain
        /// (hatched) instead of painting an assumed-ON span straight to
        /// "now". Self-healing on reconnect: fresh heartbeats advance the
        /// boundary and any backfilled winlog (shutdown / boot) repaints
        /// the gap definitively.
        #[serde(skip_serializing_if = "Option::is_none")]
        last_heartbeat: Option<DateTime<Utc>>,
    },
}

#[derive(Serialize)]
pub struct WidgetResult {
    pub dashboard: String,
    pub title: String,
    /// Optional muted subtitle for the SPA (carried from the spec).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `"pc"` or `"fleet"` — the scope this result was computed at.
    pub scope: &'static str,
    /// Whether this widget is pinned to the main Dashboard. Carried so the
    /// Analytics page can show a "pinned" affordance; the Dashboard fetches
    /// with `?pinned=true` and only ever sees `true` here.
    pub pin_dashboard: bool,
    #[serde(flatten)]
    pub data: WidgetData,
}

pub async fn get(
    State(state): State<AppState>,
    Query(q): Query<AnalyticsQuery>,
) -> Result<Json<Vec<WidgetResult>>, StatusCode> {
    // Issue #1126: `from`/`to` gate the string-stored `at` column via
    // byte-wise comparison, so an expanded-year bound would invert the
    // window instead of narrowing it. Reject before defaults are applied.
    if !bounds_in_range([q.from, q.to]) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or(to - Duration::hours(24));
    if from >= to {
        return Err(StatusCode::BAD_REQUEST);
    }
    let tz_off = q.tz_offset_minutes.unwrap_or(0).clamp(-900, 900);
    let tz_mod = format!("{tz_off:+} minutes");
    // pc_id present ⇒ per-PC widgets; absent ⇒ fleet widgets.
    let want_scope = if q.pc_id.is_some() {
        AggregateScope::Pc
    } else {
        AggregateScope::Fleet
    };
    let scope_str = if q.pc_id.is_some() { "pc" } else { "fleet" };
    let ctx = Ctx {
        pool: &state.pool,
        pc_id: q.pc_id.as_deref(),
        from,
        to,
        tz_mod: &tz_mod,
    };

    let mut widgets = load_widgets(&state.jetstream).await;
    // Render order (#743): explicit `order` weight first (absent ⇒ 0), then
    // the alphabetical (dashboard, title) fallback. Sorting the specs up
    // front means the compute loop preserves it (one bad/skipped widget
    // keeps the rest in order) and the SPA derives tab order from it.
    widgets.sort_by(|a, b| widget_sort_key(a).cmp(&widget_sort_key(b)));
    let only_pinned = q.pinned.unwrap_or(false);
    let mut out = Vec::new();
    for w in widgets {
        if w.scope != want_scope {
            // A pinned widget that's the wrong scope for this request would
            // otherwise vanish without a trace — the Dashboard fetches fleet
            // scope, so a `pin_dashboard: true` + `scope: pc` widget is dropped
            // here. Leave a debug breadcrumb (not warn — this handler polls
            // every ~30s) so a misconfigured pin is diagnosable; the field doc
            // also tells operators to pin fleet-scope widgets.
            if only_pinned && w.pin_dashboard {
                debug!(
                    dashboard = %w.dashboard, title = %w.title,
                    "analytics: pinned widget skipped — pin a fleet-scope widget for the Dashboard",
                );
            }
            continue;
        }
        // Dashboard pinned section asks for `?pinned=true` and gets only the
        // promoted widgets; the Analytics page omits the param and gets all.
        if only_pinned && !w.pin_dashboard {
            continue;
        }
        match compute_widget(&ctx, &w).await {
            Ok(Some(data)) => out.push(WidgetResult {
                dashboard: w.dashboard,
                title: w.title,
                description: w.description,
                scope: scope_str,
                pin_dashboard: w.pin_dashboard,
                data,
            }),
            // A widget whose enums fell through to the #492 Unknown
            // catch-all (a future variant this build doesn't understand)
            // is skipped, not failed.
            Ok(None) => {}
            // One bad widget shouldn't 500 the whole page — log and drop.
            Err(e) => {
                warn!(error = %e, dashboard = %w.dashboard, title = %w.title, "analytics: widget compute")
            }
        }
    }
    // SQL-backed view widgets (#vuln-roadmap PR3) query the projector tables
    // directly (inventory / feeds / …). Each widget is fleet-global OR per-PC:
    // a per-PC widget's query binds `:pc_id` (see `view_sql::is_per_pc`) and
    // renders only in the per-PC scope, bound to the selected PC; a fleet
    // widget renders only in the fleet scope. So the widget's scope must match
    // the request scope — exactly like the obs_events widgets above. Each is
    // served from the materialization cache (keyed by pc for per-PC widgets),
    // so an expensive correlation join doesn't re-run on every ~30s poll.
    for (view_id, idx, w) in load_sql_widgets(&state.jetstream).await {
        let widget_scope = if view_sql::is_per_pc(&w) {
            AggregateScope::Pc
        } else {
            AggregateScope::Fleet
        };
        if widget_scope != want_scope {
            // `validate_sql_widgets` rejects a pinned per-PC widget at create
            // time, but a hand-poked KV entry could still carry one — leave the
            // same diagnostic breadcrumb the obs_events path does (not warn:
            // this handler polls ~30s) so a dead pin is at least traceable.
            if only_pinned && w.placement.is_pinned() && widget_scope == AggregateScope::Pc {
                debug!(
                    view = %view_id, title = %w.title,
                    "analytics: pinned per-PC sql widget skipped — the Dashboard is fleet-scope",
                );
            }
            continue;
        }
        // A per-PC widget can't pin to the fleet Dashboard (it needs a selected
        // PC), so `?pinned=true` (Dashboard, fleet scope) only ever reaches
        // fleet widgets here — the scope guard above already dropped per-PC
        // ones. The pinned filter still applies to fleet widgets.
        if only_pinned && !w.placement.is_pinned() {
            continue;
        }
        let bound_pc = if widget_scope == AggregateScope::Pc {
            q.pc_id.as_deref()
        } else {
            None
        };
        match view_sql::compute_cached(&state, &view_id, idx, &w, bound_pc).await {
            Ok(data) => out.push(WidgetResult {
                dashboard: w.placement.tab().to_string(),
                title: w.title,
                description: w.description,
                scope: scope_str,
                pin_dashboard: w.placement.is_pinned(),
                data,
            }),
            // One bad view widget (a query error, a missing column) shouldn't
            // 500 the page — log and drop, like the obs path.
            Err(e) => {
                warn!(error = %e, view = %view_id, title = %w.title, "analytics: sql widget compute")
            }
        }
    }
    // `widgets` was pre-sorted, so `out` is already in render order.
    Ok(Json(out))
}

/// Render-order key for a widget (#743): explicit `order` weight (absent
/// ⇒ 0), then the alphabetical (dashboard, title) fallback. So a fleet
/// with no `order` anywhere stays purely alphabetical, and a lower `order`
/// pulls a widget — and, via first-appearance, its tab — earlier.
fn widget_sort_key(w: &AggregateWidget) -> (i32, &str, &str) {
    (w.order.unwrap_or(0), &w.dashboard, &w.title)
}

/// Collect every aggregate widget the fleet declares, merging two sources
/// (#743): the co-located `aggregate:` hint on each **job** in
/// `BUCKET_JOBS` (a job charting its own emitted data), and standalone
/// **view** resources in `BUCKET_VIEWS` (cross-cutting dashboards that
/// reference kinds emitted by other jobs / the agent). An unreadable or
/// missing entry / bucket is skipped, not fatal.
///
/// No dedup across the two sources: the same `(dashboard, title)` defined
/// in both a job hint and a view renders twice (the visible duplicate is
/// the signal — define a widget in one place). Silently dropping one copy
/// could hide a diverging config, so we surface both rather than guess.
async fn load_widgets(jetstream: &async_nats::jetstream::Context) -> Vec<AggregateWidget> {
    let mut out = Vec::new();
    // Job hints.
    if let Ok(kv) = jetstream.get_key_value(BUCKET_JOBS).await
        && let Ok(mut keys) = kv.keys().await
    {
        while let Some(key) = keys.next().await {
            let Ok(key) = key else { continue };
            let Some(entry) = kv.get(&key).await.unwrap_or(None) else {
                continue;
            };
            if let Ok(job) = serde_json::from_slice::<Manifest>(&entry)
                && let Some(widgets) = job.aggregate
            {
                out.extend(widgets);
            }
        }
    }
    // Standalone views.
    if let Ok(kv) = jetstream.get_key_value(BUCKET_VIEWS).await
        && let Ok(mut keys) = kv.keys().await
    {
        while let Some(key) = keys.next().await {
            let Ok(key) = key else { continue };
            let Some(entry) = kv.get(&key).await.unwrap_or(None) else {
                continue;
            };
            if let Ok(view) = serde_json::from_slice::<View>(&entry) {
                out.extend(view.widgets);
            }
        }
    }
    out
}

/// Collect every SQL-backed widget across the standalone view resources in
/// `BUCKET_VIEWS`, tagged with its `(view_id, index)` so the materialization
/// cache can key each one. An unreadable entry / bucket is skipped, not fatal
/// (mirrors [`load_widgets`]).
async fn load_sql_widgets(
    jetstream: &async_nats::jetstream::Context,
) -> Vec<(String, usize, SqlWidget)> {
    let mut out = Vec::new();
    if let Ok(kv) = jetstream.get_key_value(BUCKET_VIEWS).await
        && let Ok(mut keys) = kv.keys().await
    {
        while let Some(key) = keys.next().await {
            let Ok(key) = key else { continue };
            let Some(entry) = kv.get(&key).await.unwrap_or(None) else {
                continue;
            };
            if let Ok(view) = serde_json::from_slice::<View>(&entry) {
                let id = view.id;
                for (i, w) in view.sql_widgets.into_iter().enumerate() {
                    out.push((id.clone(), i, w));
                }
            }
        }
    }
    out
}

/// Compute one widget into its render payload, or `None` when its spec
/// uses a forward-compat `Unknown` enum this build can't execute.
async fn compute_widget(ctx: &Ctx<'_>, w: &AggregateWidget) -> anyhow::Result<Option<WidgetData>> {
    // Skip a widget whose spec uses any #492 `Unknown` catch-all (a future
    // variant this build can't execute) — across every enum field, not
    // just agg/render, so a future `transform`/`time_bucket` is dropped
    // cleanly rather than silently misrouted.
    if matches!(w.agg, Some(AggregateAgg::Unknown))
        || matches!(w.render, AggregateRender::Unknown)
        || matches!(w.transform, Some(AggregateTransform::Unknown))
        || matches!(w.time_bucket, Some(AggregateTimeBucket::Unknown))
    {
        return Ok(None);
    }

    // `op_timeline` performs no rollup — it returns the window's raw
    // operational events for the SPA to fold into lane spans. Handled before
    // the agg dispatch because it reads neither `kind` nor `agg`.
    if matches!(w.render, AggregateRender::OpTimeline) {
        return Ok(Some(op_timeline(ctx).await?));
    }
    let exclude_json = if w.exclude.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&w.exclude)?)
    };
    let limit = w.limit.map(i64::from).unwrap_or(DEFAULT_LIMIT);
    let sample = w.sample_minutes.map(i64::from);

    // A time bucket always renders a timeline, regardless of agg.
    if matches!(w.time_bucket, Some(AggregateTimeBucket::Hour)) {
        return Ok(Some(timeline(ctx, w).await?));
    }

    let data = match w.agg {
        Some(AggregateAgg::Ratio) => gauge(ctx, w, sample).await?,
        // pc_id ranking is matched before the host transform so a stored
        // `group_by: pc_id` + `transform: host` (nonsense, and rejected at
        // create time) can't misroute into bar_host("pc_id").
        Some(AggregateAgg::Count) => match &w.group_by {
            Some(gb) if gb == "pc_id" => bar_count_pc(ctx, w, exclude_json, limit, sample).await?,
            Some(gb) if matches!(w.transform, Some(AggregateTransform::Host)) => {
                bar_host(ctx, w, gb, limit, sample).await?
            }
            Some(gb) => bar_count_path(ctx, w, gb, exclude_json, limit, sample).await?,
            None => stat_count(ctx, w, sample).await?,
        },
        Some(AggregateAgg::Sum) => {
            let vp = w.value_path.as_deref().unwrap_or_default();
            match &w.group_by {
                Some(gb) if gb == "pc_id" => sum_bar_pc(ctx, w, vp, exclude_json, limit).await?,
                Some(gb) => sum_bar_path(ctx, w, gb, vp, exclude_json, limit).await?,
                None => sum_stat(ctx, w, vp).await?,
            }
        }
        // `None` (op_timeline, handled above; or a malformed stored widget
        // missing `agg`), Unknown (handled above), + any future variant.
        _ => return Ok(None),
    };
    Ok(Some(data))
}

/// `agg: count` + `group_by: <json path>` → top-N bars by row count.
async fn bar_count_path(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    path: &str,
    exclude_json: Option<String>,
    limit: i64,
    sample: Option<i64>,
) -> anyhow::Result<WidgetData> {
    let rows = sqlx::query(
        "SELECT json_extract(payload, '$.' || ?6) AS g, COUNT(*) AS n \
         FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5 \
           AND json_extract(payload, '$.' || ?6) IS NOT NULL \
           AND json_extract(payload, '$.' || ?6) <> '' \
           AND (?7 IS NULL OR json_extract(payload, '$.' || ?6) NOT IN (SELECT value FROM json_each(?7))) \
         GROUP BY json_extract(payload, '$.' || ?6) ORDER BY n DESC LIMIT ?8",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .bind(path)
    .bind(exclude_json)
    .bind(limit)
    .fetch_all(ctx.pool)
    .await?;
    Ok(WidgetData::Bar {
        rows: rows
            .into_iter()
            .filter_map(|r| bar_row(&r, sample))
            .collect(),
    })
}

/// `agg: count` + `group_by: pc_id` → fleet ranking of PCs by row count.
async fn bar_count_pc(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    exclude_json: Option<String>,
    limit: i64,
    sample: Option<i64>,
) -> anyhow::Result<WidgetData> {
    let rows = sqlx::query(
        "SELECT pc_id AS g, COUNT(*) AS n \
         FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5 \
           AND (?6 IS NULL OR pc_id NOT IN (SELECT value FROM json_each(?6))) \
         GROUP BY pc_id ORDER BY n DESC LIMIT ?7",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .bind(exclude_json)
    .bind(limit)
    .fetch_all(ctx.pool)
    .await?;
    Ok(WidgetData::Bar {
        rows: rows
            .into_iter()
            .filter_map(|r| bar_row(&r, sample))
            .collect(),
    })
}

/// `agg: count` + `transform: host` → pull the day's values (capped) and
/// fold them into host counts in Rust (SQLite can't parse a URL). Mirrors
/// the old utilization top-sites. `exclude` is applied post-host.
async fn bar_host(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    path: &str,
    limit: i64,
    sample: Option<i64>,
) -> anyhow::Result<WidgetData> {
    let rows = sqlx::query(
        "SELECT json_extract(payload, '$.' || ?6) AS v \
         FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5 \
           AND json_extract(payload, '$.' || ?6) IS NOT NULL \
         ORDER BY at DESC LIMIT ?7",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .bind(path)
    .bind(MAX_RAW_ROWS)
    .fetch_all(ctx.pool)
    .await?;
    let excluded: std::collections::HashSet<&str> = w.exclude.iter().map(String::as_str).collect();
    let mut counts: HashMap<String, i64> = HashMap::new();
    for r in &rows {
        let v: String = r.try_get("v").unwrap_or_default();
        if let Some(host) = host_of(&v) {
            if excluded.contains(host.as_str()) {
                continue;
            }
            *counts.entry(host).or_insert(0) += 1;
        }
    }
    let mut ranked: Vec<(String, i64)> = counts.into_iter().collect();
    // Count desc, then host asc to make the cut deterministic on ties.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(limit.max(0) as usize);
    Ok(WidgetData::Bar {
        rows: ranked
            .into_iter()
            .map(|(label, value)| BarRow {
                label,
                value,
                est_minutes: sample.map(|m| value * m),
            })
            .collect(),
    })
}

/// `agg: count` with no `group_by` → a single total (stat).
async fn stat_count(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    sample: Option<i64>,
) -> anyhow::Result<WidgetData> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .fetch_one(ctx.pool)
    .await?;
    let value: i64 = row.try_get("n").unwrap_or(0);
    Ok(WidgetData::Stat {
        value,
        est_minutes: sample.map(|m| value * m),
    })
}

/// `agg: ratio` over `bool_path` → a gauge (true/total + first/last).
async fn gauge(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    sample: Option<i64>,
) -> anyhow::Result<WidgetData> {
    let bool_path = w.bool_path.as_deref().unwrap_or_default();
    let row = sqlx::query(
        "SELECT COUNT(*) AS total, \
                COALESCE(SUM(CASE WHEN json_extract(payload, '$.' || ?6) = 1 THEN 1 ELSE 0 END), 0) AS active, \
                MIN(CASE WHEN json_extract(payload, '$.' || ?6) = 1 THEN at END) AS first_at, \
                MAX(CASE WHEN json_extract(payload, '$.' || ?6) = 1 THEN at END) AS last_at \
         FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .bind(bool_path)
    .fetch_one(ctx.pool)
    .await?;
    let total: i64 = row.try_get("total").unwrap_or(0);
    let active: i64 = row.try_get("active").unwrap_or(0);
    Ok(WidgetData::Gauge {
        total,
        active,
        ratio: if total > 0 {
            active as f64 / total as f64
        } else {
            0.0
        },
        est_minutes: sample.map(|m| active * m),
        first: row
            .try_get::<Option<DateTime<Utc>>, _>("first_at")
            .unwrap_or(None),
        last: row
            .try_get::<Option<DateTime<Utc>>, _>("last_at")
            .unwrap_or(None),
    })
}

/// `time_bucket: hour` → local hour-of-day strip. With a `bool_path`
/// it's an active/total ratio per hour (presence); without one it's pure
/// volume (active = total so the bars fill).
async fn timeline(ctx: &Ctx<'_>, w: &AggregateWidget) -> anyhow::Result<WidgetData> {
    // Two static SQLs rather than one: the ratio form evaluates the
    // bool_path; the volume form (no bool_path) must NOT — binding an empty
    // path would make `json_extract(payload, '$.')` (a malformed path that
    // errors in SQLite). Volume sets active = total so the bars fill.
    let rows = match w.bool_path.as_deref() {
        Some(bool_path) => {
            sqlx::query(
                "SELECT CAST(strftime('%H', at, ?1) AS INTEGER) AS hour, \
                        COUNT(*) AS total, \
                        COALESCE(SUM(CASE WHEN json_extract(payload, '$.' || ?7) = 1 THEN 1 ELSE 0 END), 0) AS active \
                 FROM obs_events \
                 WHERE (?2 IS NULL OR pc_id = ?2) AND kind = ?3 AND (?4 IS NULL OR source = ?4) \
                   AND at >= ?5 AND at < ?6 \
                 GROUP BY hour ORDER BY hour",
            )
            .bind(ctx.tz_mod)
            .bind(ctx.pc_id)
            .bind(w.kind.as_deref())
            .bind(w.source.as_deref())
            .bind(ctx.from)
            .bind(ctx.to)
            .bind(bool_path)
            .fetch_all(ctx.pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT CAST(strftime('%H', at, ?1) AS INTEGER) AS hour, \
                        COUNT(*) AS total, COUNT(*) AS active \
                 FROM obs_events \
                 WHERE (?2 IS NULL OR pc_id = ?2) AND kind = ?3 AND (?4 IS NULL OR source = ?4) \
                   AND at >= ?5 AND at < ?6 \
                 GROUP BY hour ORDER BY hour",
            )
            .bind(ctx.tz_mod)
            .bind(ctx.pc_id)
            .bind(w.kind.as_deref())
            .bind(w.source.as_deref())
            .bind(ctx.from)
            .bind(ctx.to)
            .fetch_all(ctx.pool)
            .await?
        }
    };
    // A `bool_path` makes `active` a real active/total ratio (presence);
    // without one the SQL set `active = total`, so the strip is pure volume
    // and the SPA must read bar HEIGHT as the hour's count, not a fill.
    let metric = if w.bool_path.is_some() {
        "ratio"
    } else {
        "count"
    };
    let buckets = rows
        .into_iter()
        .filter_map(|r| {
            let hour: i64 = r.try_get("hour").ok()?;
            let total: i64 = r.try_get("total").unwrap_or(0);
            let active: i64 = r.try_get("active").unwrap_or(0);
            Some(HourBucket {
                hour,
                total,
                active,
            })
        })
        .collect();
    Ok(WidgetData::Timeline { metric, buckets })
}

/// `render: op_timeline` → the per-PC operational events for the SPA to fold
/// into power / session / sleep / active lane spans. No rollup, but NOT a naive
/// `[from, to)` slice: a PC that booted (or logged on) before the window and
/// stayed up for the whole window emits no event inside it, so a plain slice
/// would render an empty lane — indistinguishable from "powered off". To let
/// the SPA reconstruct the boundary state, each lane is also seeded with its
/// single latest event *before* `from`. The window function partitions the
/// pre-window events by lane and keeps the newest per lane; everything is
/// merged and returned ascending so the SPA walks start/end pairs in one pass.
///
/// Per-PC: the widget validates `scope: pc`, so this is only reached with a
/// `pc_id` — enforce it (a missing pc_id mustn't become a full-fleet scan).
/// The kind/lane lists are literal `IN (…)` / `CASE` so the SQL stays static
/// (no dynamic assembly) and must stay aligned with `OP_TIMELINE_KINDS` /
/// `OP_LANES` in the SPA's `OperationalTimeline.tsx` — see
/// [`tests::op_timeline_kind_set_is_stable`].
async fn op_timeline(ctx: &Ctx<'_>) -> anyhow::Result<WidgetData> {
    let pc_id = ctx
        .pc_id
        .ok_or_else(|| anyhow::anyhow!("op_timeline requires a pc_id"))?;
    let rows = sqlx::query(
        "WITH op AS ( \
           SELECT at, kind, \
                  CASE \
                    WHEN kind IN ('boot', 'shutdown', 'unexpected_shutdown', \
                                  'log_service_started', 'log_service_stopped') THEN 'power' \
                    WHEN kind IN ('logon', 'logoff') THEN 'session' \
                    WHEN kind IN ('sleep', 'resume') THEN 'sleep' \
                    WHEN kind IN ('active', 'idle') THEN 'active' \
                  END AS lane \
           FROM obs_events \
           WHERE pc_id = ?1 \
             AND kind IN ('boot', 'shutdown', 'unexpected_shutdown', \
                          'log_service_started', 'log_service_stopped', \
                          'logon', 'logoff', 'sleep', 'resume', \
                          'active', 'idle', \
                          'agent_offline', 'agent_online') \
             AND at < ?3 \
         ), seeded AS ( \
           SELECT at, kind FROM op WHERE at >= ?2 \
           UNION ALL \
           SELECT at, kind FROM ( \
             SELECT at, kind, ROW_NUMBER() OVER (PARTITION BY lane ORDER BY at DESC) AS rn \
             FROM op WHERE at < ?2 \
           ) WHERE rn = 1 \
         ) \
         SELECT at, kind FROM seeded ORDER BY at",
    )
    .bind(pc_id)
    .bind(ctx.from)
    .bind(ctx.to)
    .fetch_all(ctx.pool)
    .await?;
    let events = rows
        .into_iter()
        .filter_map(|r| {
            let at: DateTime<Utc> = r.try_get("at").ok()?;
            let kind: String = r.try_get("kind").ok()?;
            Some(OpEvent { at, kind })
        })
        .collect();
    // Best-effort read of the agent's last heartbeat so the SPA can gate the
    // live edge (state after it is unconfirmed). A missing agents row (never
    // heartbeated) or a read hiccup degrades to None — the SPA then paints as
    // before rather than the whole widget failing over a soft signal. `.ok()`
    // + double-`flatten` folds Err / no-row / SQL-NULL all to None. Log the Err
    // path first so a real regression (e.g. a renamed column / missing agents
    // table after a bad migration) stays observable instead of hiding behind
    // the benign "no agents row yet" case that degrades identically.
    let last_heartbeat = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT last_heartbeat FROM agents WHERE pc_id = ?1",
    )
    .bind(pc_id)
    .fetch_optional(ctx.pool)
    .await
    .inspect_err(
        |e| debug!(pc_id = %pc_id, error = %e, "op_timeline: last_heartbeat read failed; live-edge gating disabled"),
    )
    .ok()
    .flatten()
    .flatten();
    Ok(WidgetData::OpTimeline {
        from: ctx.from,
        to: ctx.to,
        events,
        last_heartbeat,
    })
}

/// `agg: sum` + `group_by: <json path>` → top-N bars by summed value.
async fn sum_bar_path(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    path: &str,
    value_path: &str,
    exclude_json: Option<String>,
    limit: i64,
) -> anyhow::Result<WidgetData> {
    let rows = sqlx::query(
        "SELECT json_extract(payload, '$.' || ?6) AS g, \
                COALESCE(SUM(json_extract(payload, '$.' || ?7)), 0) AS s \
         FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5 \
           AND json_extract(payload, '$.' || ?6) IS NOT NULL \
           AND (?8 IS NULL OR json_extract(payload, '$.' || ?6) NOT IN (SELECT value FROM json_each(?8))) \
         GROUP BY json_extract(payload, '$.' || ?6) ORDER BY s DESC LIMIT ?9",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .bind(path)
    .bind(value_path)
    .bind(exclude_json)
    .bind(limit)
    .fetch_all(ctx.pool)
    .await?;
    Ok(WidgetData::Bar {
        rows: rows.into_iter().filter_map(|r| sum_row(&r)).collect(),
    })
}

/// `agg: sum` + `group_by: pc_id` → fleet ranking by summed value.
async fn sum_bar_pc(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    value_path: &str,
    exclude_json: Option<String>,
    limit: i64,
) -> anyhow::Result<WidgetData> {
    let rows = sqlx::query(
        "SELECT pc_id AS g, COALESCE(SUM(json_extract(payload, '$.' || ?6)), 0) AS s \
         FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5 \
           AND (?7 IS NULL OR pc_id NOT IN (SELECT value FROM json_each(?7))) \
         GROUP BY pc_id ORDER BY s DESC LIMIT ?8",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .bind(value_path)
    .bind(exclude_json)
    .bind(limit)
    .fetch_all(ctx.pool)
    .await?;
    Ok(WidgetData::Bar {
        rows: rows.into_iter().filter_map(|r| sum_row(&r)).collect(),
    })
}

/// `agg: sum` with no `group_by` → a single summed total (stat).
async fn sum_stat(
    ctx: &Ctx<'_>,
    w: &AggregateWidget,
    value_path: &str,
) -> anyhow::Result<WidgetData> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(json_extract(payload, '$.' || ?6)), 0) AS s FROM obs_events \
         WHERE (?1 IS NULL OR pc_id = ?1) AND kind = ?2 AND (?3 IS NULL OR source = ?3) \
           AND at >= ?4 AND at < ?5",
    )
    .bind(ctx.pc_id)
    .bind(w.kind.as_deref())
    .bind(w.source.as_deref())
    .bind(ctx.from)
    .bind(ctx.to)
    .bind(value_path)
    .fetch_one(ctx.pool)
    .await?;
    Ok(WidgetData::Stat {
        value: sum_value(&row),
        est_minutes: None,
    })
}

/// A `(g, n)` count row → a [`BarRow`], dropping a null/blank group.
fn bar_row(r: &sqlx::sqlite::SqliteRow, sample: Option<i64>) -> Option<BarRow> {
    let label: String = r.try_get("g").ok()?;
    let value: i64 = r.try_get("n").unwrap_or(0);
    Some(BarRow {
        label,
        value,
        est_minutes: sample.map(|m| value * m),
    })
}

/// A `(g, s)` sum row → a [`BarRow`]. `SUM` of JSON numbers can be
/// fractional; we round to a whole unit (bytes/counts) for display.
fn sum_row(r: &sqlx::sqlite::SqliteRow) -> Option<BarRow> {
    let label: String = r.try_get("g").ok()?;
    Some(BarRow {
        label,
        value: sum_value(r),
        est_minutes: None,
    })
}

/// Read a `SUM(...)` column as i64, tolerating SQLite returning it as a
/// float (json numbers) or integer.
fn sum_value(r: &sqlx::sqlite::SqliteRow) -> i64 {
    r.try_get::<i64, _>("s")
        .or_else(|_| r.try_get::<f64, _>("s").map(|f| f.round() as i64))
        .unwrap_or(0)
}

/// Best-effort host (registrable-ish authority) from a URL string, done
/// in Rust because SQLite has no URL parser. Strips scheme,
/// path/query/fragment, userinfo and port; lowercases. `None` for a
/// blank/non-navigational value so it's dropped from the rollup.
fn host_of(url: &str) -> Option<String> {
    let lower = url.trim_start().to_ascii_lowercase();
    for scheme in [
        "about:",
        "data:",
        "javascript:",
        "chrome:",
        "chrome-extension:",
        "edge:",
        "brave:",
        "view-source:",
        "file:",
    ] {
        if lower.starts_with(scheme) {
            return None;
        }
    }
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let no_userinfo = authority.rsplit('@').next().unwrap_or(authority);
    let host = if no_userinfo.starts_with('[') {
        match no_userinfo.rfind(']') {
            Some(i) => &no_userinfo[..=i],
            None => no_userinfo,
        }
    } else {
        no_userinfo.split(':').next().unwrap_or(no_userinfo)
    };
    let host = host.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn host_of_extracts_authority() {
        assert_eq!(
            host_of("https://user:pw@Example.com:8443/x?y#z").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:8080/").as_deref(), Some("[::1]"));
        assert_eq!(host_of("github.com/foo").as_deref(), Some("github.com"));
        assert_eq!(host_of("about:blank"), None);
        assert_eq!(host_of("chrome-extension://abc/page"), None);
        assert_eq!(host_of("   "), None);
    }

    // A bare obs_events table so the SQL — json_extract paths, GROUP BY,
    // exclude via json_each, strftime bucketing — runs for real (a SQL
    // typo or WHERE-alias bug only shows up against a live DB; #714).
    async fn seeded_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE obs_events ( \
               id INTEGER PRIMARY KEY AUTOINCREMENT, pc_id TEXT NOT NULL, \
               at TIMESTAMP NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, \
               event_record_id TEXT, payload TEXT )",
        )
        .execute(&pool)
        .await
        .unwrap();
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 6, 17, h, 0, 0).unwrap();
        for (pc, t, kind, payload) in [
            ("p1", at(9), "presence", r#"{"active":true}"#),
            ("p1", at(10), "presence", r#"{"active":true}"#),
            ("p1", at(11), "presence", r#"{"active":false}"#),
            ("p1", at(12), "presence", r#"{"active":false}"#),
            (
                "p1",
                at(9),
                "app_sample",
                r#"{"foreground":{"app":"brave"}}"#,
            ),
            (
                "p1",
                at(10),
                "app_sample",
                r#"{"foreground":{"app":"brave"}}"#,
            ),
            (
                "p1",
                at(11),
                "app_sample",
                r#"{"foreground":{"app":"brave"}}"#,
            ),
            (
                "p1",
                at(12),
                "app_sample",
                r#"{"foreground":{"app":"code"}}"#,
            ),
            (
                "p1",
                at(13),
                "app_sample",
                r#"{"foreground":{"app":"LockApp"}}"#,
            ),
            (
                "p1",
                at(9),
                "web_visit",
                r#"{"url":"https://github.com/a"}"#,
            ),
            (
                "p1",
                at(10),
                "web_visit",
                r#"{"url":"https://github.com/b"}"#,
            ),
            (
                "p1",
                at(11),
                "web_visit",
                r#"{"url":"https://example.com/"}"#,
            ),
            (
                "p2",
                at(9),
                "app_sample",
                r#"{"foreground":{"app":"brave"}}"#,
            ),
        ] {
            sqlx::query(
                "INSERT INTO obs_events (pc_id, at, kind, source, payload) VALUES (?,?,?,?,?)",
            )
            .bind(pc)
            .bind(t)
            .bind(kind)
            .bind("test")
            .bind(payload)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    fn widget(kind: &str, agg: AggregateAgg, render: AggregateRender) -> AggregateWidget {
        AggregateWidget {
            dashboard: "D".into(),
            title: "T".into(),
            description: None,
            order: None,
            pin_dashboard: false,
            scope: AggregateScope::Pc,
            kind: Some(kind.into()),
            source: None,
            agg: Some(agg),
            group_by: None,
            bool_path: None,
            value_path: None,
            transform: None,
            sample_minutes: None,
            exclude: Vec::new(),
            time_bucket: None,
            limit: None,
            render,
        }
    }

    fn ctx<'a>(pool: &'a SqlitePool, pc_id: Option<&'a str>) -> Ctx<'a> {
        Ctx {
            pool,
            pc_id,
            from: Utc.with_ymd_and_hms(2026, 6, 17, 0, 0, 0).unwrap(),
            to: Utc.with_ymd_and_hms(2026, 6, 18, 0, 0, 0).unwrap(),
            tz_mod: "+0 minutes",
        }
    }

    #[test]
    fn widget_sort_key_orders_by_order_then_alpha() {
        let mut ws = [
            {
                let mut w = widget("k", AggregateAgg::Count, AggregateRender::Stat);
                w.dashboard = "Utilization".into();
                w.title = "B".into();
                w
            },
            {
                let mut w = widget("k", AggregateAgg::Count, AggregateRender::Stat);
                w.dashboard = "Utilization".into();
                w.title = "A".into();
                w
            },
            {
                // Explicit low order pulls this first despite "Z"/"Zzz".
                let mut w = widget("k", AggregateAgg::Count, AggregateRender::Stat);
                w.dashboard = "Zzz".into();
                w.title = "Z".into();
                w.order = Some(-1);
                w
            },
        ];
        ws.sort_by(|a, b| widget_sort_key(a).cmp(&widget_sort_key(b)));
        let got: Vec<(&str, &str)> = ws
            .iter()
            .map(|w| (w.dashboard.as_str(), w.title.as_str()))
            .collect();
        // order:-1 first; then the order:0 pair alphabetical by title.
        assert_eq!(
            got,
            [("Zzz", "Z"), ("Utilization", "A"), ("Utilization", "B")]
        );
    }

    #[tokio::test]
    async fn count_bar_groups_excludes_and_estimates_time() {
        let pool = seeded_pool().await;
        let mut w = widget("app_sample", AggregateAgg::Count, AggregateRender::Bar);
        w.group_by = Some("foreground.app".into());
        w.exclude = vec!["LockApp".into()];
        w.sample_minutes = Some(2);
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::Bar { rows } = data else {
            panic!("expected bar")
        };
        // brave (3) then code (1); LockApp excluded; p2's brave not counted.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "brave");
        assert_eq!(rows[0].value, 3);
        assert_eq!(rows[0].est_minutes, Some(6));
        assert_eq!(rows[1].label, "code");
    }

    #[tokio::test]
    async fn ratio_gauge_counts_true_over_total() {
        let pool = seeded_pool().await;
        let mut w = widget("presence", AggregateAgg::Ratio, AggregateRender::Gauge);
        w.bool_path = Some("active".into());
        w.sample_minutes = Some(5);
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::Gauge {
            total,
            active,
            ratio,
            est_minutes,
            ..
        } = data
        else {
            panic!("expected gauge")
        };
        assert_eq!(total, 4);
        assert_eq!(active, 2);
        assert!((ratio - 0.5).abs() < 1e-9);
        assert_eq!(est_minutes, Some(10));
    }

    #[tokio::test]
    async fn host_transform_folds_urls_in_rust() {
        let pool = seeded_pool().await;
        let mut w = widget("web_visit", AggregateAgg::Count, AggregateRender::Bar);
        w.group_by = Some("url".into());
        w.transform = Some(AggregateTransform::Host);
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::Bar { rows } = data else {
            panic!("expected bar")
        };
        assert_eq!(rows[0].label, "github.com");
        assert_eq!(rows[0].value, 2);
        assert_eq!(rows[1].label, "example.com");
        assert_eq!(rows[1].value, 1);
    }

    #[tokio::test]
    async fn count_stat_is_grand_total() {
        let pool = seeded_pool().await;
        let w = widget("app_sample", AggregateAgg::Count, AggregateRender::Stat);
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::Stat { value, .. } = data else {
            panic!("expected stat")
        };
        // All 5 p1 app_sample rows (LockApp included — stat has no exclude).
        assert_eq!(value, 5);
    }

    #[tokio::test]
    async fn fleet_pc_ranking_counts_all_pcs() {
        let pool = seeded_pool().await;
        let mut w = widget("app_sample", AggregateAgg::Count, AggregateRender::Bar);
        w.scope = AggregateScope::Fleet;
        w.group_by = Some("pc_id".into());
        // Fleet scope ⇒ pc_id filter is None.
        let data = compute_widget(&ctx(&pool, None), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::Bar { rows } = data else {
            panic!("expected bar")
        };
        assert_eq!(rows[0].label, "p1"); // 5 samples
        assert_eq!(rows[0].value, 5);
        assert_eq!(rows[1].label, "p2"); // 1 sample
    }

    #[tokio::test]
    async fn ratio_timeline_buckets_by_local_hour() {
        let pool = seeded_pool().await;
        let mut w = widget("presence", AggregateAgg::Ratio, AggregateRender::Timeline);
        w.bool_path = Some("active".into());
        w.time_bucket = Some(AggregateTimeBucket::Hour);
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::Timeline { metric, buckets } = data else {
            panic!("expected timeline")
        };
        // A bool_path widget is a presence ratio strip.
        assert_eq!(metric, "ratio");
        // Hours 9 & 10 active, 11 & 12 inactive — one bucket each (UTC).
        let h9 = buckets.iter().find(|b| b.hour == 9).unwrap();
        assert_eq!((h9.total, h9.active), (1, 1));
        let h11 = buckets.iter().find(|b| b.hour == 11).unwrap();
        assert_eq!((h11.total, h11.active), (1, 0));
    }

    #[tokio::test]
    async fn count_timeline_reports_volume_metric() {
        // No bool_path → the SQL sets active = total, so the strip is pure
        // volume: metric must be "count" so the SPA scales bar height by the
        // hour's magnitude rather than mislabelling it active/idle.
        let pool = seeded_pool().await;
        let mut w = widget("app_sample", AggregateAgg::Count, AggregateRender::Timeline);
        w.time_bucket = Some(AggregateTimeBucket::Hour);
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::Timeline { metric, buckets } = data else {
            panic!("expected timeline")
        };
        assert_eq!(metric, "count");
        // Volume mode mirrors active into total (active = total per hour).
        assert!(buckets.iter().all(|b| b.active == b.total));
        // p1 has one app_sample per hour at 9..13, so each populated hour
        // carries a real count the SPA can scale by.
        let h9 = buckets.iter().find(|b| b.hour == 9).unwrap();
        assert_eq!(h9.total, 1);
    }

    #[tokio::test]
    async fn timeline_shifts_into_local_hours() {
        // tz +540 (JST) shifts the UTC 09:00 active sample to local 18:00,
        // exercising the strftime modifier binding (a `{tz_off:+} minutes`
        // bug would only show with a non-zero offset).
        let pool = seeded_pool().await;
        let mut c = ctx(&pool, Some("p1"));
        c.tz_mod = "+540 minutes";
        let mut w = widget("presence", AggregateAgg::Ratio, AggregateRender::Timeline);
        w.bool_path = Some("active".into());
        w.time_bucket = Some(AggregateTimeBucket::Hour);
        let data = compute_widget(&c, &w).await.unwrap().unwrap();
        let WidgetData::Timeline { buckets, .. } = data else {
            panic!("expected timeline")
        };
        // UTC 09:00 (+9h) → local 18:00; nothing left in UTC-hour 9.
        assert!(buckets.iter().any(|b| b.hour == 18 && b.active == 1));
        assert!(buckets.iter().all(|b| b.hour != 9));
    }

    #[tokio::test]
    async fn op_timeline_returns_operational_events_in_window() {
        // op_timeline returns the window's operational events for the chosen
        // PC, ascending — non-operational kinds (app_sample) and other PCs are
        // filtered out, and the query window is echoed back for the SPA to
        // clamp open spans against.
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE obs_events ( \
               id INTEGER PRIMARY KEY AUTOINCREMENT, pc_id TEXT NOT NULL, \
               at TIMESTAMP NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, \
               event_record_id TEXT, payload TEXT )",
        )
        .execute(&pool)
        .await
        .unwrap();
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 6, 17, h, 0, 0).unwrap();
        for (pc, t, kind) in [
            ("p1", at(8), "boot"),
            ("p1", at(9), "logon"),
            ("p1", at(12), "sleep"),
            ("p1", at(13), "resume"),
            ("p1", at(18), "logoff"),
            ("p1", at(19), "shutdown"),
            ("p1", at(10), "app_sample"), // non-operational → excluded
            ("p2", at(9), "boot"),        // other PC → excluded by pc filter
        ] {
            sqlx::query(
                "INSERT INTO obs_events (pc_id, at, kind, source, payload) VALUES (?,?,?,?,?)",
            )
            .bind(pc)
            .bind(t)
            .bind(kind)
            .bind("test")
            .bind("{}")
            .execute(&pool)
            .await
            .unwrap();
        }
        // op_timeline ignores kind/agg; a real spec leaves them unset.
        let mut w = widget("ignored", AggregateAgg::Count, AggregateRender::OpTimeline);
        w.kind = None;
        w.agg = None;
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::OpTimeline {
            from, to, events, ..
        } = data
        else {
            panic!("expected op_timeline")
        };
        assert_eq!(from, Utc.with_ymd_and_hms(2026, 6, 17, 0, 0, 0).unwrap());
        assert_eq!(to, Utc.with_ymd_and_hms(2026, 6, 18, 0, 0, 0).unwrap());
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["boot", "logon", "sleep", "resume", "logoff", "shutdown"]
        );
    }

    #[tokio::test]
    async fn op_timeline_requires_pc_id() {
        // op_timeline is per-PC; a ctx with no pc_id is a misuse and must
        // error rather than silently scan the whole fleet.
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE obs_events ( \
               id INTEGER PRIMARY KEY AUTOINCREMENT, pc_id TEXT NOT NULL, \
               at TIMESTAMP NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, \
               event_record_id TEXT, payload TEXT )",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut w = widget("ignored", AggregateAgg::Count, AggregateRender::OpTimeline);
        w.kind = None;
        w.agg = None;
        // `WidgetData` isn't `Debug`, so match rather than `unwrap_err()`.
        match compute_widget(&ctx(&pool, None), &w).await {
            Ok(_) => panic!("op_timeline must error without a pc_id"),
            Err(e) => assert!(e.to_string().contains("requires a pc_id"), "err: {e}"),
        }
    }

    // A bare obs_events table for the op_timeline queries (the seeded_pool
    // above carries presence/app_sample/web_visit, not operational kinds).
    async fn op_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE obs_events ( \
               id INTEGER PRIMARY KEY AUTOINCREMENT, pc_id TEXT NOT NULL, \
               at TIMESTAMP NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, \
               event_record_id TEXT, payload TEXT )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn insert_op(pool: &SqlitePool, pc: &str, at: DateTime<Utc>, kind: &str) {
        sqlx::query("INSERT INTO obs_events (pc_id, at, kind, source, payload) VALUES (?,?,?,?,?)")
            .bind(pc)
            .bind(at)
            .bind(kind)
            .bind("test")
            .bind("{}")
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn op_timeline_carries_last_heartbeat_from_agents_row() {
        // Happy path: when the PC has an agents row, its last_heartbeat rides
        // along on the widget so the SPA can gate the live edge. The other
        // op_timeline tests stand up only obs_events, which exercises the
        // graceful degrade-to-None branch (no agents table → read errors →
        // None); this pins the populated case so a regression there is caught.
        let pool = op_pool().await;
        sqlx::query("CREATE TABLE agents (pc_id TEXT PRIMARY KEY, last_heartbeat TIMESTAMP)")
            .execute(&pool)
            .await
            .unwrap();
        let hb = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        sqlx::query("INSERT INTO agents (pc_id, last_heartbeat) VALUES (?, ?)")
            .bind("p1")
            .bind(hb)
            .execute(&pool)
            .await
            .unwrap();
        insert_op(
            &pool,
            "p1",
            Utc.with_ymd_and_hms(2026, 6, 17, 8, 0, 0).unwrap(),
            "boot",
        )
        .await;
        let mut w = widget("ignored", AggregateAgg::Count, AggregateRender::OpTimeline);
        w.kind = None;
        w.agg = None;
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::OpTimeline { last_heartbeat, .. } = data else {
            panic!("expected op_timeline")
        };
        assert_eq!(last_heartbeat, Some(hb));
    }

    #[tokio::test]
    async fn op_timeline_kind_set_is_stable() {
        // The op_timeline IN-list must stay aligned with OP_TIMELINE_KINDS /
        // OP_LANES in the SPA's OperationalTimeline.tsx — a lane added on one
        // side but not the other silently drops events. This pins the backend
        // half: every operational kind round-trips and a non-operational kind
        // (app_sample) is excluded. If you change the set, change it in the
        // SPA too.
        let pool = op_pool().await;
        let mut want = [
            "boot",
            "shutdown",
            "unexpected_shutdown",
            "log_service_started",
            "log_service_stopped",
            "logon",
            "logoff",
            "sleep",
            "resume",
            "active",
            "idle",
            // Observation kinds: they drive no lane (the CASE leaves their
            // lane NULL) but the strip needs them to know when nobody was
            // watching, so they must survive the fetch.
            "agent_offline",
            "agent_online",
        ];
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 6, 17, h, 0, 0).unwrap();
        for (i, k) in want.iter().enumerate() {
            insert_op(&pool, "p1", at(i as u32), k).await;
        }
        insert_op(&pool, "p1", at(20), "app_sample").await;
        let mut w = widget("ignored", AggregateAgg::Count, AggregateRender::OpTimeline);
        w.kind = None;
        w.agg = None;
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::OpTimeline { events, .. } = data else {
            panic!("expected op_timeline")
        };
        let mut got: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn op_timeline_seeds_last_pre_window_event_per_lane() {
        // A PC that booted / logged on before the window and stayed up emits
        // nothing inside it — without a boundary seed the lane would render
        // empty (looks powered-off). The query seeds the single latest
        // pre-`from` event per lane; an older pre-window event is dropped.
        let pool = op_pool().await;
        let at = |d: u32, h: u32| Utc.with_ymd_and_hms(2026, 6, d, h, 0, 0).unwrap();
        // Window is 6/17 all day (ctx default). Pre-window power events:
        insert_op(&pool, "p1", at(15, 9), "boot").await; // older → dropped
        insert_op(&pool, "p1", at(16, 9), "shutdown").await; // latest power → seeded
        insert_op(&pool, "p1", at(16, 10), "logon").await; // latest session → seeded
        // In-window event:
        insert_op(&pool, "p1", at(17, 12), "logoff").await;
        let mut w = widget("ignored", AggregateAgg::Count, AggregateRender::OpTimeline);
        w.kind = None;
        w.agg = None;
        let data = compute_widget(&ctx(&pool, Some("p1")), &w)
            .await
            .unwrap()
            .unwrap();
        let WidgetData::OpTimeline { events, .. } = data else {
            panic!("expected op_timeline")
        };
        let pairs: Vec<(DateTime<Utc>, &str)> =
            events.iter().map(|e| (e.at, e.kind.as_str())).collect();
        // Latest pre-window event per lane (shutdown, logon) + the in-window
        // logoff, ascending. The 6/15 boot is the older power event → dropped.
        assert_eq!(
            pairs,
            [
                (at(16, 9), "shutdown"),
                (at(16, 10), "logon"),
                (at(17, 12), "logoff"),
            ]
        );
    }
}
