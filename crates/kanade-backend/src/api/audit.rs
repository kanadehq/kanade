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
    /// Regex applied to the `action` column. Plain text without regex
    /// metacharacters behaves like a substring search.
    pub action: Option<String>,
    /// Exact-match filter on `actor` — the SPA dropdown supplies the
    /// bounded set (`scheduler` / `operator` / `self-update` / `agent`).
    pub actor: Option<String>,
    /// Regex on the `target` column (job id, schedule id, …).
    pub target: Option<String>,
    /// Regex on the JSON-serialized `payload`.
    pub payload: Option<String>,
    /// ISO-8601 lower bound on `occurred_at`. Anything strictly older is filtered out.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_limit() -> u32 {
    50
}

// Upper bound on the prefilter window when at least one regex filter
// is active: SQL narrows by actor + since first, then we ORDER BY
// occurred_at DESC and scan up to MAX_FETCH rows in Rust, applying the
// compiled regexes and stopping once `limit` matches are collected.
// sqlx 0.8 doesn't expose `create_scalar_function` for SQLite, so a
// native REGEXP UDF isn't on the table; pulling the prefilter into
// Rust is the simpler path. With the default 24h window an audit log
// would need to exceed ~10k events per day for this cap to bite.
const MAX_FETCH: i64 = 10_000;

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<AuditRow>>, (StatusCode, String)> {
    let action_re = super::compile(params.action.as_deref())?;
    let target_re = super::compile(params.target.as_deref())?;
    let payload_re = super::compile(params.payload.as_deref())?;
    let has_regex = action_re.is_some() || target_re.is_some() || payload_re.is_some();

    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM audit_log");
    let mut sep = " WHERE ";

    if let Some(actor) = params.actor.as_deref().filter(|s| !s.is_empty()) {
        qb.push(sep).push("actor = ").push_bind(actor.to_owned());
        sep = " AND ";
    }
    if let Some(since) = params.since {
        qb.push(sep).push("occurred_at >= ").push_bind(since);
        sep = " AND ";
    }
    let _ = sep;

    qb.push(" ORDER BY occurred_at DESC LIMIT ");
    let sql_limit = if has_regex {
        MAX_FETCH
    } else {
        params.limit as i64
    };
    qb.push_bind(sql_limit);

    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
        warn!(error = %e, "list audit");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "list audit failed".to_string(),
        )
    })?;

    let limit = params.limit as usize;
    let mut out: Vec<AuditRow> = Vec::with_capacity(limit.min(64));
    for r in rows {
        let row = row_to_audit(r);
        if let Some(re) = &action_re
            && !re.is_match(&row.action)
        {
            continue;
        }
        if let Some(re) = &target_re
            && !re.is_match(row.target.as_deref().unwrap_or(""))
        {
            continue;
        }
        if let Some(re) = &payload_re {
            let s = serde_json::to_string(&row.payload).unwrap_or_default();
            if !re.is_match(&s) {
                continue;
            }
        }
        out.push(row);
        if out.len() >= limit {
            break;
        }
    }
    Ok(Json(out))
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
