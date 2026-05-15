use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tracing::warn;

#[derive(Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub payload: serde_json::Value,
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub action: Option<String>,
    pub actor: Option<String>,
}

fn default_limit() -> u32 {
    50
}

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<AuditRow>>, StatusCode> {
    let rows = match (&params.action, &params.actor) {
        (Some(a), Some(actor)) => {
            sqlx::query(
                "SELECT * FROM audit_log WHERE action = ? AND actor = ?
                 ORDER BY occurred_at DESC LIMIT ?",
            )
            .bind(a)
            .bind(actor)
            .bind(params.limit as i64)
            .fetch_all(&pool)
            .await
        }
        (Some(a), None) => {
            sqlx::query(
                "SELECT * FROM audit_log WHERE action = ?
                 ORDER BY occurred_at DESC LIMIT ?",
            )
            .bind(a)
            .bind(params.limit as i64)
            .fetch_all(&pool)
            .await
        }
        (None, Some(actor)) => {
            sqlx::query(
                "SELECT * FROM audit_log WHERE actor = ?
                 ORDER BY occurred_at DESC LIMIT ?",
            )
            .bind(actor)
            .bind(params.limit as i64)
            .fetch_all(&pool)
            .await
        }
        (None, None) => {
            sqlx::query("SELECT * FROM audit_log ORDER BY occurred_at DESC LIMIT ?")
                .bind(params.limit as i64)
                .fetch_all(&pool)
                .await
        }
    }
    .map_err(|e| {
        warn!(error = %e, "list audit");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(rows.into_iter().map(row_to_audit).collect()))
}

fn row_to_audit(r: sqlx::sqlite::SqliteRow) -> AuditRow {
    let payload_str: Option<String> = r.try_get("payload").ok();
    let payload = payload_str
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    AuditRow {
        id: r.try_get("id").unwrap_or(0),
        actor: r.try_get("actor").unwrap_or_default(),
        action: r.try_get("action").unwrap_or_default(),
        target: r.try_get("target").ok(),
        payload,
        occurred_at: r.try_get("occurred_at").ok(),
    }
}
