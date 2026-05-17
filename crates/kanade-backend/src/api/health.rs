//! `GET /api/health/fleet` — machine-readable rollup of fleet
//! health for external monitors (Nagios, Prometheus blackbox,
//! k8s liveness probes, alerting cron).
//!
//! Returns JSON describing the agent inventory freshness, the
//! JetStream resource set, and recent deployment failures. HTTP
//! status mirrors `status`:
//!
//!   * `ok`        → 200 — all resources present, no stale agents
//!   * `unknown`   → 200 — no agents reporting yet (fresh install)
//!   * `degraded`  → 503 — at least one JetStream resource missing
//!     OR one or more agents have gone stale
//!
//! The endpoint sits under `/api/*` so it inherits the same auth
//! middleware as the rest of the admin surface; if your monitor
//! can't carry a bearer token, hit the unauthenticated `/health`
//! liveness probe instead.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_SCHEDULES,
    BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, STREAM_AUDIT,
    STREAM_DEPLOY, STREAM_EVENTS, STREAM_INVENTORY, STREAM_RESULTS,
};
use serde::Serialize;
use sqlx::Row;
use tracing::warn;

use super::AppState;

/// Stale threshold for `last_heartbeat`. Heartbeats cadence at 30 s
/// by default; 2 min of slack catches a few missed ticks without
/// flapping during a single packet drop. Matches the Dashboard's
/// "active" rollup.
const STALE_THRESHOLD: Duration = Duration::minutes(2);

/// Look-back window for the recent-failure rollup.
const RECENT_WINDOW: Duration = Duration::hours(24);

#[derive(Serialize)]
pub struct FleetHealth {
    pub status: HealthStatus,
    pub agents: AgentsHealth,
    pub jetstream: JetstreamHealth,
    pub recent_results: RecentResults,
    pub observed_at: DateTime<Utc>,
}

#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Ok,
    Unknown,
    Degraded,
}

#[derive(Serialize)]
pub struct AgentsHealth {
    pub known: i64,
    pub active: i64,
    pub stale: i64,
}

#[derive(Serialize)]
pub struct JetstreamHealth {
    pub all_ok: bool,
    pub healthy: usize,
    pub total: usize,
    pub missing: Vec<String>,
}

#[derive(Serialize)]
pub struct RecentResults {
    pub window_hours: i64,
    pub total: i64,
    pub failed: i64,
}

pub async fn fleet(State(state): State<AppState>) -> (StatusCode, Json<FleetHealth>) {
    let now = Utc::now();
    let stale_cutoff = now - STALE_THRESHOLD;
    let recent_cutoff = now - RECENT_WINDOW;

    let agents = agents_health(&state.pool, stale_cutoff).await;
    let jetstream = jetstream_health(&state.jetstream).await;
    let recent_results = recent_results(&state.pool, recent_cutoff).await;

    let status = if jetstream.all_ok && agents.stale == 0 && agents.known > 0 {
        HealthStatus::Ok
    } else if !jetstream.all_ok || agents.stale > 0 {
        HealthStatus::Degraded
    } else {
        HealthStatus::Unknown
    };

    let code = match status {
        HealthStatus::Ok | HealthStatus::Unknown => StatusCode::OK,
        HealthStatus::Degraded => StatusCode::SERVICE_UNAVAILABLE,
    };

    (
        code,
        Json(FleetHealth {
            status,
            agents,
            jetstream,
            recent_results,
            observed_at: now,
        }),
    )
}

async fn agents_health(pool: &sqlx::SqlitePool, stale_cutoff: DateTime<Utc>) -> AgentsHealth {
    let row = sqlx::query(
        "SELECT
             COUNT(*) AS known,
             COALESCE(SUM(CASE WHEN last_heartbeat >= ? THEN 1 ELSE 0 END), 0) AS active
         FROM agents",
    )
    .bind(stale_cutoff)
    .fetch_one(pool)
    .await;
    let (known, active) = match row {
        Ok(r) => (
            r.try_get::<i64, _>("known").unwrap_or(0),
            r.try_get::<i64, _>("active").unwrap_or(0),
        ),
        Err(e) => {
            warn!(error = %e, "agents_health query");
            (0, 0)
        }
    };
    AgentsHealth {
        known,
        active,
        stale: (known - active).max(0),
    }
}

async fn jetstream_health(js: &async_nats::jetstream::Context) -> JetstreamHealth {
    let mut missing = Vec::new();
    let mut total = 0usize;

    for name in [
        STREAM_INVENTORY,
        STREAM_RESULTS,
        STREAM_DEPLOY,
        STREAM_EVENTS,
        STREAM_AUDIT,
    ] {
        total += 1;
        if js.get_stream(name).await.is_err() {
            missing.push(name.to_string());
        }
    }
    for name in [
        BUCKET_SCRIPT_CURRENT,
        BUCKET_SCRIPT_STATUS,
        BUCKET_AGENTS_STATE,
        BUCKET_AGENT_CONFIG,
        BUCKET_AGENT_GROUPS,
        BUCKET_SCHEDULES,
    ] {
        total += 1;
        if js.get_key_value(name).await.is_err() {
            missing.push(name.to_string());
        }
    }
    for name in [OBJECT_AGENT_RELEASES] {
        total += 1;
        if js.get_object_store(name).await.is_err() {
            missing.push(name.to_string());
        }
    }

    JetstreamHealth {
        all_ok: missing.is_empty(),
        healthy: total - missing.len(),
        total,
        missing,
    }
}

async fn recent_results(pool: &sqlx::SqlitePool, since: DateTime<Utc>) -> RecentResults {
    let row = sqlx::query(
        "SELECT
             COUNT(*) AS total,
             COALESCE(SUM(CASE WHEN exit_code <> 0 THEN 1 ELSE 0 END), 0) AS failed
         FROM deployment_results
         WHERE recorded_at >= ?",
    )
    .bind(since)
    .fetch_one(pool)
    .await;
    let (total, failed) = match row {
        Ok(r) => (
            r.try_get::<i64, _>("total").unwrap_or(0),
            r.try_get::<i64, _>("failed").unwrap_or(0),
        ),
        Err(e) => {
            warn!(error = %e, "recent_results query");
            (0, 0)
        }
    };
    RecentResults {
        window_hours: RECENT_WINDOW.num_hours(),
        total,
        failed,
    }
}
