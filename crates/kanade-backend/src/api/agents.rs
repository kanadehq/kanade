use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tracing::warn;

/// v0.14: the agents table is now baseline-only. The fields are
/// populated by the heartbeat projector — pc_id / hostname /
/// os_family / agent_version / last_heartbeat. For richer
/// per-host facts (CPU / RAM / disks / OS detail / installed
/// software / ...) consult the `inventory_facts` table via
/// `GET /api/inventory/<pc_id>`; each operator-defined probe
/// (manifest with an `inventory:` hint) lands its
/// `ConvertTo-Json` output there.
#[derive(Serialize)]
pub struct AgentRow {
    pub pc_id: String,
    pub hostname: Option<String>,
    pub os_family: Option<String>,
    pub agent_version: Option<String>,
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// v0.37 Part 2: agent process self-perf — populated by the
    /// heartbeat projector when the agent supplies these (a pre-
    /// 0.37 agent's heartbeat omits them and the field stays None).
    /// `agent_cpu_pct` is a percent-of-one-core (sysinfo
    /// convention; 200 on a process pegging 2 cores). `*_bytes`
    /// fields are absolute since process start; the SPA diffs
    /// successive snapshots locally if it wants rates.
    pub agent_cpu_pct: Option<f64>,
    pub agent_rss_bytes: Option<i64>,
    pub agent_disk_read_bytes: Option<i64>,
    pub agent_disk_written_bytes: Option<i64>,
}

pub async fn list(State(pool): State<SqlitePool>) -> Result<Json<Vec<AgentRow>>, StatusCode> {
    let rows = sqlx::query("SELECT * FROM agents ORDER BY updated_at DESC")
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "list agents");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(rows.into_iter().map(row_to_agent).collect()))
}

pub async fn detail(
    State(pool): State<SqlitePool>,
    Path(pc_id): Path<String>,
) -> Result<Json<AgentRow>, StatusCode> {
    let row = sqlx::query("SELECT * FROM agents WHERE pc_id = ?")
        .bind(&pc_id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "detail agent");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    match row {
        Some(r) => Ok(Json(row_to_agent(r))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

fn row_to_agent(r: sqlx::sqlite::SqliteRow) -> AgentRow {
    AgentRow {
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        hostname: r.try_get("hostname").ok(),
        os_family: r.try_get("os_family").ok(),
        agent_version: r.try_get("agent_version").ok(),
        last_heartbeat: r.try_get("last_heartbeat").ok(),
        updated_at: r.try_get("updated_at").ok(),
        agent_cpu_pct: r.try_get("agent_cpu_pct").ok(),
        agent_rss_bytes: r.try_get("agent_rss_bytes").ok(),
        agent_disk_read_bytes: r.try_get("agent_disk_read_bytes").ok(),
        agent_disk_written_bytes: r.try_get("agent_disk_written_bytes").ok(),
    }
}
