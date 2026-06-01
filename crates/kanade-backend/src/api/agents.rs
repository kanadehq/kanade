use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
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

/// Query params for `GET /api/agents`.
///
/// Both default to the historical "whole fleet" behaviour when
/// omitted, so existing callers (the Agents table) keep working
/// untouched. The SPA's shared PcPicker passes both: a typed `q`
/// plus a small `limit`, so a 3000-host fleet only ever streams the
/// handful of typeahead candidates instead of the full table.
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    /// Case-insensitive substring match against `pc_id` OR
    /// `hostname`. Absent / empty → no filter.
    pub q: Option<String>,
    /// Cap on rows returned. Absent → unbounded (the full list).
    pub limit: Option<u32>,
}

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<AgentRow>>, StatusCode> {
    // Turn `q` into a bound LIKE pattern, escaping the LIKE
    // metacharacters so a host literally named `pc_1` or `web%` is
    // matched verbatim rather than as a wildcard. `\` is the escape
    // char (declared via ESCAPE below).
    let like = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let escaped = s
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        });

    // `?2` is the row cap; SQLite treats a negative LIMIT as
    // "unbounded", so the omitted-limit path binds -1 and keeps the
    // SQL a single static string (sqlx 0.9 rejects dynamically-built
    // query strings).
    let limit = params.limit.map(i64::from).unwrap_or(-1);

    let rows = sqlx::query(
        "SELECT * FROM agents \
         WHERE (?1 IS NULL OR pc_id LIKE ?1 ESCAPE '\\' OR hostname LIKE ?1 ESCAPE '\\') \
         ORDER BY updated_at DESC \
         LIMIT ?2",
    )
    .bind(like)
    .bind(limit)
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn seeded_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for (pc, host) in [
            ("PC001", "alpha"),
            ("PC002", "beta"),
            ("WS-9", "gamma"),
            ("web%01", "delta"),
        ] {
            sqlx::query("INSERT INTO agents (pc_id, hostname) VALUES (?, ?)")
                .bind(pc)
                .bind(host)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    async fn ids(pool: SqlitePool, q: Option<&str>, limit: Option<u32>) -> Vec<String> {
        let Json(rows) = list(
            State(pool),
            Query(ListParams {
                q: q.map(Into::into),
                limit,
            }),
        )
        .await
        .unwrap();
        rows.into_iter().map(|r| r.pc_id).collect()
    }

    #[tokio::test]
    async fn no_query_returns_whole_fleet() {
        let got = ids(seeded_pool().await, None, None).await;
        assert_eq!(got.len(), 4);
    }

    #[tokio::test]
    async fn blank_query_is_treated_as_no_filter() {
        let got = ids(seeded_pool().await, Some("   "), None).await;
        assert_eq!(got.len(), 4);
    }

    #[tokio::test]
    async fn filters_by_pc_id_substring() {
        let mut got = ids(seeded_pool().await, Some("pc00"), None).await;
        got.sort();
        assert_eq!(got, vec!["PC001".to_string(), "PC002".to_string()]);
    }

    #[tokio::test]
    async fn matches_hostname_too() {
        let got = ids(seeded_pool().await, Some("gamma"), None).await;
        assert_eq!(got, vec!["WS-9".to_string()]);
    }

    #[tokio::test]
    async fn like_metacharacters_match_literally() {
        // `%` must match the host literally named `web%01`, not act as
        // a wildcard that would sweep in every row.
        let got = ids(seeded_pool().await, Some("web%0"), None).await;
        assert_eq!(got, vec!["web%01".to_string()]);
    }

    #[tokio::test]
    async fn limit_caps_row_count() {
        let got = ids(seeded_pool().await, None, Some(2)).await;
        assert_eq!(got.len(), 2);
    }
}
