use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
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
    /// #495: rows to skip — server-side paging for the Agents table.
    /// Absent → 0. The pre-LIMIT match count rides back in the
    /// `X-Total-Count` response header so the body stays a plain
    /// `Vec<AgentRow>` and existing consumers (PcPicker, Dashboard)
    /// are untouched.
    pub offset: Option<u32>,
    /// #563: `"online"` / `"offline"` liveness filter, evaluated
    /// server-side against [`ALIVE_THRESHOLD`] so the Dashboard's
    /// `/agents?status=offline` deep link pages over the WHOLE
    /// fleet's offline hosts, not just the current page. Absent /
    /// empty → no filter; anything else → 400.
    pub status: Option<String>,
}

/// Heartbeat age past which an agent counts as offline. The single
/// source of truth shared by this filter, the scheduler's expected-
/// PC resolution, and (numerically — it hardcodes 2 min) the SPA's
/// `isAgentOnline`.
pub const ALIVE_THRESHOLD: chrono::Duration = chrono::Duration::minutes(2);

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<(HeaderMap, Json<Vec<AgentRow>>), StatusCode> {
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

    // #563: validate the status filter up front — a typo'd value
    // silently meaning "all" would defeat the deep link's purpose.
    let status = match params.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s @ ("online" | "offline")) => Some(s.to_string()),
        Some(_) => return Err(StatusCode::BAD_REQUEST),
    };
    // One liveness instant for the whole request so the counts and
    // the page rows agree about an agent sitting on the threshold.
    let cutoff = chrono::Utc::now() - ALIVE_THRESHOLD;

    // `?4` is the row cap; SQLite treats a negative LIMIT as
    // "unbounded", so the omitted-limit path binds -1 and keeps the
    // SQL a single static string (sqlx 0.9 rejects dynamically-built
    // query strings).
    let limit = params.limit.map(i64::from).unwrap_or(-1);
    let offset = params.offset.map(i64::from).unwrap_or(0);

    // #495/#563: pre-LIMIT counts for the paging header + the
    // status chips. One aggregate pass computes the q-matching
    // total AND its online share, so the chips can show fleet-wide
    // per-status numbers no matter which filter is active. The
    // agents table is one row per PC (bounded by fleet size), so
    // this is cheap — and skipped entirely for unbounded callers
    // (PcPicker / Dashboard pass no limit, so the row count IS the
    // total; PR #559 review, gemini).
    let needs_count = params.limit.is_some();
    let (matched, online): (i64, i64) = if !needs_count {
        (0, 0)
    } else {
        let row = sqlx::query(
            "SELECT COUNT(*) AS matched, \
                    CAST(COALESCE(SUM(CASE WHEN last_heartbeat IS NOT NULL \
                                            AND last_heartbeat >= ?2 \
                                           THEN 1 ELSE 0 END), 0) AS INTEGER) AS online \
               FROM agents \
              WHERE (?1 IS NULL OR pc_id LIKE ?1 ESCAPE '\\' OR hostname LIKE ?1 ESCAPE '\\')",
        )
        .bind(&like)
        .bind(cutoff)
        .fetch_one(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "count agents");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        (
            row.try_get("matched").unwrap_or(0),
            row.try_get("online").unwrap_or(0),
        )
    };

    let rows = sqlx::query(
        "SELECT * FROM agents \
         WHERE (?1 IS NULL OR pc_id LIKE ?1 ESCAPE '\\' OR hostname LIKE ?1 ESCAPE '\\') \
           AND (?2 IS NULL \
                OR (?2 = 'online' AND last_heartbeat IS NOT NULL AND last_heartbeat >= ?3) \
                OR (?2 = 'offline' AND (last_heartbeat IS NULL OR last_heartbeat < ?3))) \
         ORDER BY updated_at DESC \
         LIMIT ?4 OFFSET ?5",
    )
    .bind(&like)
    .bind(&status)
    .bind(cutoff)
    .bind(limit)
    .bind(offset)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        warn!(error = %e, "list agents");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // X-Total-Count reflects the ACTIVE filter (it drives paging);
    // the per-status counts ride alongside so the chips stay
    // fleet-wide-correct whichever chip is selected.
    let total: i64 = if needs_count {
        match status.as_deref() {
            Some("online") => online,
            Some("offline") => matched - online,
            _ => matched,
        }
    } else {
        offset + rows.len() as i64
    };
    let mut headers = HeaderMap::new();
    if let Ok(v) = total.to_string().parse() {
        headers.insert("X-Total-Count", v);
    }
    if needs_count {
        if let Ok(v) = online.to_string().parse() {
            headers.insert("X-Online-Count", v);
        }
        if let Ok(v) = (matched - online).to_string().parse() {
            headers.insert("X-Offline-Count", v);
        }
    }
    Ok((headers, Json(rows.into_iter().map(row_to_agent).collect())))
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
        let (_headers, Json(rows)) = list(
            State(pool),
            Query(ListParams {
                q: q.map(Into::into),
                limit,
                offset: None,
                status: None,
            }),
        )
        .await
        .unwrap();
        rows.into_iter().map(|r| r.pc_id).collect()
    }

    /// #563: mark a seeded agent online (heartbeat = now) or
    /// long-offline (heartbeat = 1h ago); unset rows stay NULL.
    async fn set_heartbeat(pool: &SqlitePool, pc_id: &str, online: bool) {
        let hb = if online {
            chrono::Utc::now()
        } else {
            chrono::Utc::now() - chrono::Duration::hours(1)
        };
        sqlx::query("UPDATE agents SET last_heartbeat = ? WHERE pc_id = ?")
            .bind(hb)
            .bind(pc_id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn status_filter_is_server_side_and_counts_are_fleet_wide() {
        let pool = seeded_pool().await;
        // PC001 online; PC002 stale; WS-9 / web%01 never heartbeated
        // (NULL) — both NULL and stale count as offline.
        set_heartbeat(&pool, "PC001", true).await;
        set_heartbeat(&pool, "PC002", false).await;

        let (headers, Json(rows)) = list(
            State(pool),
            Query(ListParams {
                q: None,
                limit: Some(2),
                offset: None,
                status: Some("offline".into()),
            }),
        )
        .await
        .unwrap();
        let get = |h: &HeaderMap, k: &str| -> i64 {
            h.get(k)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| panic!("{k} header missing or unparseable"))
        };
        // Page is offline-only and capped by limit…
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.pc_id != "PC001"));
        // …while X-Total-Count reflects the active filter (3 offline
        // fleet-wide — paging works past page 1) and the chip counts
        // are fleet-wide regardless of the filter.
        assert_eq!(get(&headers, "X-Total-Count"), 3);
        assert_eq!(get(&headers, "X-Online-Count"), 1);
        assert_eq!(get(&headers, "X-Offline-Count"), 3);
    }

    #[tokio::test]
    async fn online_filter_returns_only_live_agents() {
        let pool = seeded_pool().await;
        set_heartbeat(&pool, "PC001", true).await;
        let (headers, Json(rows)) = list(
            State(pool),
            Query(ListParams {
                q: None,
                limit: Some(10),
                offset: None,
                status: Some("online".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.pc_id.as_str()).collect::<Vec<_>>(),
            vec!["PC001"]
        );
        let total: i64 = headers
            .get("X-Total-Count")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap();
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn invalid_status_is_a_bad_request() {
        let pool = seeded_pool().await;
        // `unwrap_err` would need AgentRow: Debug — match instead.
        match list(
            State(pool),
            Query(ListParams {
                q: None,
                limit: Some(10),
                offset: None,
                status: Some("onlin".into()),
            }),
        )
        .await
        {
            Err(code) => assert_eq!(code, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("a typo'd status must be a 400, not silently 'all'"),
        }
    }

    #[tokio::test]
    async fn offset_pages_and_total_header_reports_match_count() {
        // #495: server-side paging — page 2 skips page 1's rows, and
        // X-Total-Count carries the pre-LIMIT match count.
        let pool = seeded_pool().await;
        let (headers, Json(page2)) = list(
            State(pool),
            Query(ListParams {
                q: None,
                limit: Some(1),
                offset: Some(1),
                status: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(page2.len(), 1);
        let total: i64 = headers
            .get("X-Total-Count")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .expect("total header present");
        assert_eq!(total, 4, "seeded fleet has exactly four agents");
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
