//! `GET /api/utilization/{pc_id}` — per-PC activity rollup over a time
//! window, aggregated from `obs_events`. Generic: it summarises whatever
//! `presence` / `app_sample` / `web_visit` events a PC has emitted (the
//! attendance collectors produce these, but nothing here is specific to
//! them). Drives the SPA Utilization page.
//!
//! `?from=&to=` are RFC3339 UTC bounds (from inclusive, to exclusive);
//! the SPA computes them from the operator's chosen LOCAL day so the
//! day boundary is correct in their timezone. Both omitted ⇒ last 24h.
//!
//! Returns an active summary, top apps, top sites, and an hourly
//! active/idle timeline (bucketed into the operator's local hours via
//! `tz_offset_minutes`).

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::warn;

/// Cap on `web_visit` rows pulled for host aggregation — a busy day is
/// well under this; the bound just stops a pathological PC from pulling
/// an unbounded set into memory.
const MAX_VISIT_ROWS: i64 = 10_000;

#[derive(Deserialize)]
pub struct WindowQuery {
    /// RFC3339 lower bound (inclusive). Default: `to` − 24h.
    pub from: Option<DateTime<Utc>>,
    /// RFC3339 upper bound (exclusive). Default: now.
    pub to: Option<DateTime<Utc>>,
    /// Minutes to ADD to a UTC `at` to get the operator's local time,
    /// used only to bucket the hourly timeline into local hours-of-day
    /// (e.g. JST = `540`). Default 0 (UTC buckets). Clamped to ±900.
    pub tz_offset_minutes: Option<i64>,
}

#[derive(Serialize)]
pub struct ActiveSummary {
    /// `presence` samples in the window.
    pub total_samples: i64,
    /// …of which `active` (input within the snapshot's 5-min threshold).
    pub active_samples: i64,
    /// `active_samples / total_samples` (0.0 when no samples).
    pub active_ratio: f64,
    /// First / last sample marked `active` — i.e. first/last time the
    /// person was at the keyboard in the window.
    pub first_active: Option<DateTime<Utc>>,
    pub last_active: Option<DateTime<Utc>>,
    /// Rough active minutes = `active_samples × 5` (the attendance-snapshot
    /// cadence). The SPA labels this as an estimate.
    pub est_active_minutes: i64,
}

#[derive(Serialize)]
pub struct AppCount {
    pub app: String,
    pub samples: i64,
}

#[derive(Serialize)]
pub struct SiteCount {
    pub host: String,
    pub visits: i64,
}

/// One local hour-of-day bucket of presence samples for the timeline.
#[derive(Serialize)]
pub struct HourBucket {
    /// Local hour 0–23 (bucketed using `tz_offset_minutes`).
    pub hour: i64,
    pub total: i64,
    pub active: i64,
}

#[derive(Serialize)]
pub struct UtilizationResponse {
    pub pc_id: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub active: ActiveSummary,
    pub top_apps: Vec<AppCount>,
    pub top_sites: Vec<SiteCount>,
    /// True when the `web_visit` scan hit `MAX_VISIT_ROWS` — the
    /// top-sites ranking is then over a truncated set, so the SPA can
    /// warn that it's approximate.
    pub site_visits_capped: bool,
    /// Presence samples bucketed by local hour-of-day (sparse — only
    /// hours with samples appear; the SPA fills 0–23).
    pub timeline: Vec<HourBucket>,
}

/// Best-effort host (registrable-ish authority) from a URL string,
/// done in Rust because SQLite has no URL parser. Strips scheme,
/// path/query/fragment, userinfo and port; lowercases. `None` for a
/// blank/garbage value so it's dropped from the rollup.
fn host_of(url: &str) -> Option<String> {
    // Skip non-navigational browser URLs (about:blank, data:, devtools,
    // extension pages) — without a `://` they'd otherwise be mis-parsed
    // into a bogus host like "about" and pollute the top-sites ranking.
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
    // Trim the port. IPv6 literals are bracketed (`[::1]:8080`) — keep
    // through the closing bracket so the colons inside survive.
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

pub async fn get(
    State(pool): State<SqlitePool>,
    Path(pc_id): Path<String>,
    Query(q): Query<WindowQuery>,
) -> Result<Json<UtilizationResponse>, StatusCode> {
    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or_else(|| to - Duration::hours(24));
    if from >= to {
        return Err(StatusCode::BAD_REQUEST);
    }
    // strftime modifier to shift a UTC `at` into the operator's local
    // time for hour-of-day bucketing (e.g. "+540 minutes" for JST).
    let tz_off = q.tz_offset_minutes.unwrap_or(0).clamp(-900, 900);
    let tz_mod = format!("{tz_off:+} minutes");

    // ── active summary (presence) ───────────────────────────────────
    // json_extract of a JSON boolean `true` yields 1 in SQLite, so the
    // `= 1` test counts active samples. first/last consider active rows
    // only (when the person was actually at the keyboard).
    let active_row = sqlx::query(
        "SELECT \
           COUNT(*) AS total, \
           COALESCE(SUM(CASE WHEN json_extract(payload, '$.active') = 1 THEN 1 ELSE 0 END), 0) AS active, \
           MIN(CASE WHEN json_extract(payload, '$.active') = 1 THEN at END) AS first_active, \
           MAX(CASE WHEN json_extract(payload, '$.active') = 1 THEN at END) AS last_active \
         FROM obs_events \
         WHERE pc_id = ? AND kind = 'presence' AND at >= ? AND at < ?",
    )
    .bind(&pc_id)
    .bind(from)
    .bind(to)
    .fetch_one(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "utilization: presence aggregate");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let total_samples: i64 = active_row.try_get("total").unwrap_or(0);
    let active_samples: i64 = active_row.try_get("active").unwrap_or(0);
    let active = ActiveSummary {
        total_samples,
        active_samples,
        active_ratio: if total_samples > 0 {
            active_samples as f64 / total_samples as f64
        } else {
            0.0
        },
        first_active: active_row
            .try_get::<Option<DateTime<Utc>>, _>("first_active")
            .unwrap_or(None),
        last_active: active_row
            .try_get::<Option<DateTime<Utc>>, _>("last_active")
            .unwrap_or(None),
        est_active_minutes: active_samples * 5,
    };

    // ── top apps (app_sample foreground) ────────────────────────────
    // NB: SELECT aliases (`app`) aren't visible in WHERE in SQLite —
    // repeat the json_extract expression there (and in GROUP BY) rather
    // than referencing the alias, which would error at runtime.
    let app_rows = sqlx::query(
        "SELECT json_extract(payload, '$.foreground.app') AS app, COUNT(*) AS n \
         FROM obs_events \
         WHERE pc_id = ? AND kind = 'app_sample' AND at >= ? AND at < ? \
           AND json_extract(payload, '$.foreground.app') IS NOT NULL \
           AND json_extract(payload, '$.foreground.app') <> '' \
         GROUP BY json_extract(payload, '$.foreground.app') ORDER BY n DESC LIMIT 10",
    )
    .bind(&pc_id)
    .bind(from)
    .bind(to)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "utilization: app_sample aggregate");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let top_apps = app_rows
        .into_iter()
        .filter_map(|r| {
            let app: String = r.try_get("app").ok()?;
            let samples: i64 = r.try_get("n").unwrap_or(0);
            Some(AppCount { app, samples })
        })
        .collect();

    // ── top sites (web_visit) ───────────────────────────────────────
    // SQLite can't parse URLs, so pull the day's visit URLs (capped) and
    // fold them into host counts in Rust. ORDER BY at DESC makes the cap
    // deterministic — when a day exceeds MAX_VISIT_ROWS we keep the most
    // recent visits rather than an arbitrary subset (coderabbit).
    let visit_rows = sqlx::query(
        "SELECT json_extract(payload, '$.url') AS url \
         FROM obs_events \
         WHERE pc_id = ? AND kind = 'web_visit' AND at >= ? AND at < ? \
           AND json_extract(payload, '$.url') IS NOT NULL \
         ORDER BY at DESC LIMIT ?",
    )
    .bind(&pc_id)
    .bind(from)
    .bind(to)
    .bind(MAX_VISIT_ROWS)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "utilization: web_visit fetch");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let site_visits_capped = visit_rows.len() as i64 >= MAX_VISIT_ROWS;
    let mut host_counts: HashMap<String, i64> = HashMap::new();
    for r in visit_rows {
        let url: String = match r.try_get("url") {
            Ok(u) => u,
            Err(_) => continue,
        };
        if let Some(h) = host_of(&url) {
            *host_counts.entry(h).or_insert(0) += 1;
        }
    }
    let mut top_sites: Vec<SiteCount> = host_counts
        .into_iter()
        .map(|(host, visits)| SiteCount { host, visits })
        .collect();
    // Highest first, then host for a deterministic tie-break, then top 10.
    top_sites.sort_by(|a, b| b.visits.cmp(&a.visits).then_with(|| a.host.cmp(&b.host)));
    top_sites.truncate(10);

    // ── hourly timeline (presence by local hour-of-day) ─────────────
    // strftime applies the tz modifier to shift `at` into local time,
    // then '%H' gives the 0–23 hour; the SPA fills the missing hours.
    let hour_rows = sqlx::query(
        "SELECT CAST(strftime('%H', at, ?) AS INTEGER) AS hour, \
                COUNT(*) AS total, \
                COALESCE(SUM(CASE WHEN json_extract(payload, '$.active') = 1 THEN 1 ELSE 0 END), 0) AS active \
         FROM obs_events \
         WHERE pc_id = ? AND kind = 'presence' AND at >= ? AND at < ? \
         GROUP BY hour ORDER BY hour",
    )
    .bind(&tz_mod)
    .bind(&pc_id)
    .bind(from)
    .bind(to)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "utilization: timeline aggregate");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let timeline = hour_rows
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

    Ok(Json(UtilizationResponse {
        pc_id,
        from,
        to,
        active,
        top_apps,
        top_sites,
        site_visits_capped,
        timeline,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_extracts_authority() {
        assert_eq!(
            host_of("https://github.com/yukimemi/kanade").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_of("http://localhost:8080/events?kind=x").as_deref(),
            Some("localhost")
        );
        assert_eq!(
            host_of("https://user:pw@mail.google.com/chat/").as_deref(),
            Some("mail.google.com")
        );
        assert_eq!(
            host_of("HTTPS://Example.COM/").as_deref(),
            Some("example.com")
        );
        // IPv6 literal keeps its brackets (the colons inside survive).
        assert_eq!(host_of("http://[::1]:8080/").as_deref(), Some("[::1]"));
        // Non-navigational schemes are dropped so they don't pollute
        // the top-sites ranking.
        assert_eq!(host_of("about:blank"), None);
        assert_eq!(host_of("data:text/html,hi"), None);
        assert_eq!(host_of("chrome-extension://abc/page.html"), None);
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("https://"), None);
    }
}
