use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
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
    /// ISO-8601 lower bound on `occurred_at`. Anything strictly older is filtered out.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_limit() -> u32 {
    50
}

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<AuditRow>>, StatusCode> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM audit_log");
    let mut sep = " WHERE ";

    if let Some(action) = params.action.as_deref().filter(|s| !s.is_empty()) {
        qb.push(sep).push("action = ").push_bind(action.to_owned());
        sep = " AND ";
    }
    if let Some(actor) = params.actor.as_deref().filter(|s| !s.is_empty()) {
        qb.push(sep).push("actor = ").push_bind(actor.to_owned());
        sep = " AND ";
    }
    if let Some(since) = params.since {
        qb.push(sep).push("occurred_at >= ").push_bind(since);
        let _ = sep;
    }

    qb.push(" ORDER BY occurred_at DESC LIMIT ")
        .push_bind(params.limit as i64);

    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
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
