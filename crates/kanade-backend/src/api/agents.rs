use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct AgentRow {
    pub pc_id: String,
    pub hostname: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub os_build: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<i64>,
    pub ram_bytes: Option<i64>,
    pub disks: serde_json::Value,
    pub last_inventory: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
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
    let disks_str: Option<String> = r.try_get("disks_json").ok();
    let disks = disks_str
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::Value::Array(vec![]));
    AgentRow {
        pc_id: r.try_get("pc_id").unwrap_or_default(),
        hostname: r.try_get("hostname").ok(),
        os_name: r.try_get("os_name").ok(),
        os_version: r.try_get("os_version").ok(),
        os_build: r.try_get("os_build").ok(),
        cpu_model: r.try_get("cpu_model").ok(),
        cpu_cores: r.try_get("cpu_cores").ok(),
        ram_bytes: r.try_get("ram_bytes").ok(),
        disks,
        last_inventory: r.try_get("last_inventory").ok(),
        updated_at: r.try_get("updated_at").ok(),
    }
}
