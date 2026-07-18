use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use kanade_shared::wire::MetaEntry;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tracing::{info, warn};

use crate::api::AppState;
use crate::audit::{self, Caller};

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
    /// #582 Phase 2: versions this agent's boot sentinel rolled back
    /// after a crash-loop on boot (and now refuses to re-deploy).
    /// Drives the Rollout page's "failed to adopt target" view.
    /// Omitted (not `[]`) for the common clean case — most agents —
    /// matching the optional `quarantined_versions?` SPA type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined_versions: Vec<String>,
    /// #655: the account the host's Windows sign-in screen last used —
    /// `last_logon_user` is the `DOMAIN\sam` login name,
    /// `last_logon_display_name` its friendly name. Populated by the
    /// heartbeat projector; `None` for never-signed-in / pre-#655 /
    /// non-Windows agents.
    pub last_logon_user: Option<String>,
    pub last_logon_display_name: Option<String>,
    /// #1051: operator-managed key/value metadata for this PC, from the
    /// `agent_meta` projection (source of truth is the KV bucket the SPA
    /// `PUT`/`kanade meta` write). Decorated onto the returned page so the
    /// Agents list can render selected keys as columns. Omitted (not `[]`)
    /// when the PC has no metadata, keeping the common response small.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meta: Vec<MetaEntry>,
}

/// Query params for `GET /api/agents`.
///
/// All filters are optional and default to the historical "whole
/// fleet" behaviour when omitted, so existing callers (the Agents
/// table, the SPA's shared PcPicker, the Dashboard) keep working.
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    /// #652: regex over `pc_id` OR `hostname` (matches if EITHER
    /// hits). Was a LIKE substring match before; now a regex to match
    /// the Activity / Audit pages. Plain text without regex
    /// metacharacters still behaves like a substring search. Blank /
    /// whitespace-only → no filter.
    pub q: Option<String>,
    /// #652: regex over `last_logon_user` OR `last_logon_display_name`.
    pub user: Option<String>,
    /// #652: regex over `agent_version`.
    pub version: Option<String>,
    /// #652: a version that must appear in the agent's
    /// `quarantined_versions` (exact token match inside the JSON
    /// array). Drives the Rollout "quarantined K" drill-down — the
    /// link lands here with `?quarantined=<target>` so the operator
    /// sees exactly which hosts rolled the target back. Pre-filtered
    /// in SQL (a cheap LIKE on the quoted token) so it scales to a
    /// large fleet.
    pub quarantined: Option<String>,
    /// Cap on rows returned. Absent → unbounded (the full list).
    pub limit: Option<u32>,
    /// #495: rows to skip — server-side paging. Absent → 0. The
    /// pre-LIMIT match count rides back in `X-Total-Count`.
    pub offset: Option<u32>,
    /// #563: `"online"` / `"offline"` liveness filter, evaluated
    /// server-side against [`ALIVE_THRESHOLD`]. Absent / empty → no
    /// filter; anything else → 400.
    pub status: Option<String>,
    /// #1051: restrict to PCs that have this `agent_meta` key. Blank /
    /// whitespace-only → no metadata filter. Paired with `meta_value`.
    pub meta_key: Option<String>,
    /// #1051: when `meta_key` is set, require the key's value to CONTAIN
    /// this text (SQLite `LIKE '%…%'`, metacharacters escaped). Blank →
    /// "has the key with any value". Ignored when `meta_key` is blank.
    pub meta_value: Option<String>,
}

/// Heartbeat age past which an agent counts as offline. The single
/// source of truth shared by this filter, the scheduler's expected-
/// PC resolution, and (numerically — it hardcodes 2 min) the SPA's
/// `isAgentOnline`.
pub const ALIVE_THRESHOLD: chrono::Duration = chrono::Duration::minutes(2);

/// Backstop on rows pulled into the regex prefilter window. The agents
/// table is one row per PC, so even a multi-thousand-host fleet sits
/// well under this — it's a guard against pathological growth, not an
/// expected limit (cf. Activity's 10k full-text cap, #596).
const MAX_FETCH: i64 = 10_000;

/// Build the `%"<version>"%` LIKE pattern that tests whether a version
/// appears as a token inside the `quarantined_versions` JSON array
/// (`["0.43.51","0.43.62"]`). Matching the quoted token avoids a
/// version being matched as a substring of another (e.g. `0.43.6`
/// must not hit `0.43.62`). LIKE metacharacters in the version are
/// escaped (ESCAPE '\' is declared at the call site).
fn quarantined_like(version: Option<&str>) -> Option<String> {
    version.map(str::trim).filter(|s| !s.is_empty()).map(|s| {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%\"{escaped}\"%")
    })
}

/// Wrap `value` as a SQLite `LIKE '%value%'` contains-pattern with the
/// LIKE metacharacters escaped (ESCAPE '\' is declared at the call site).
fn contains_like(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// The SQL-expressible pre-filters shared by the count query, the
/// fast-path row query, and the regex-path prefilter: the quarantine
/// token LIKE and the #1051 metadata filter. `started` tracks whether a
/// `WHERE` has been emitted yet so the caller can append its own extra
/// clauses (the status filter) with the right `WHERE`/`AND` separator.
///
/// The metadata filter is a correlated subquery
/// (`pc_id IN (SELECT pc_id FROM agent_meta WHERE key = ? [AND value LIKE ?])`)
/// so it composes with paging + the fleet-wide count without pulling the
/// roster into memory.
fn push_prefilters(
    qb: &mut QueryBuilder<Sqlite>,
    started: &mut bool,
    quar_like: &Option<String>,
    meta_filter: &Option<(String, Option<String>)>,
) {
    if let Some(p) = quar_like {
        qb.push(sep(started))
            .push("quarantined_versions LIKE ")
            .push_bind(p.clone())
            .push(" ESCAPE '\\'");
    }
    if let Some((key, like)) = meta_filter {
        qb.push(sep(started))
            .push("pc_id IN (SELECT pc_id FROM agent_meta WHERE key = ")
            .push_bind(key.clone());
        if let Some(l) = like {
            qb.push(" AND value LIKE ")
                .push_bind(l.clone())
                .push(" ESCAPE '\\'");
        }
        qb.push(")");
    }
}

/// Emit `" WHERE "` for the first clause, `" AND "` thereafter.
fn sep(started: &mut bool) -> &'static str {
    if *started {
        " AND "
    } else {
        *started = true;
        " WHERE "
    }
}

/// How many pc_ids to bind per `attach_meta` IN-list query. Kept well
/// under SQLite's host-parameter cap (999 on older builds, 32766 since
/// 3.32) so an unbounded `GET /api/agents` (no `limit` → whole fleet)
/// can't trip `too many SQL variables` on a large roster. (Gemini HIGH.)
const META_IN_CHUNK: usize = 500;

/// Decorate a page of agents with their `agent_meta` rows, grouped by
/// pc_id. No-op for an empty page. The page is queried in chunks of
/// [`META_IN_CHUNK`] so the IN-list can't exceed SQLite's bound-parameter
/// limit even when the caller asked for the whole (unbounded) fleet. A
/// metadata read failure is surfaced to the caller rather than silently
/// dropping columns.
async fn attach_meta(pool: &SqlitePool, page: &mut [AgentRow]) -> Result<(), (StatusCode, String)> {
    if page.is_empty() {
        return Ok(());
    }
    let mut by_pc: std::collections::HashMap<String, Vec<MetaEntry>> =
        std::collections::HashMap::new();
    for chunk in page.chunks(META_IN_CHUNK) {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT pc_id, key, value FROM agent_meta WHERE pc_id IN (");
        {
            let mut list = qb.separated(", ");
            for a in chunk {
                list.push_bind(a.pc_id.clone());
            }
        }
        qb.push(") ORDER BY pc_id, key");
        let rows = qb.build().fetch_all(pool).await.map_err(|e| {
            warn!(error = %e, "attach agent_meta");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "load agent metadata failed".to_string(),
            )
        })?;
        for r in rows {
            let pc: String = r.try_get("pc_id").unwrap_or_default();
            let key: String = r.try_get("key").unwrap_or_default();
            let value: String = r.try_get("value").unwrap_or_default();
            by_pc.entry(pc).or_default().push(MetaEntry { key, value });
        }
    }
    for a in page.iter_mut() {
        if let Some(m) = by_pc.remove(&a.pc_id) {
            a.meta = m;
        }
    }
    Ok(())
}

fn is_online(a: &AgentRow, cutoff: chrono::DateTime<chrono::Utc>) -> bool {
    a.last_heartbeat.is_some_and(|hb| hb >= cutoff)
}

/// `X-Total-Count` reflects the ACTIVE status filter (it drives
/// paging); when no count is requested it falls back to the row tally.
fn total_count(
    needs_count: bool,
    status: Option<&str>,
    matched: i64,
    online: i64,
    fallback: i64,
) -> i64 {
    if !needs_count {
        return fallback;
    }
    match status {
        Some("online") => online,
        Some("offline") => matched - online,
        _ => matched,
    }
}

/// `X-Total-Count` + (when counting) the fleet-wide per-status chip
/// counts. The chip counts ignore the active status filter so the
/// chips stay correct whichever one is selected.
fn build_headers(needs_count: bool, total: i64, matched: i64, online: i64) -> HeaderMap {
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
    headers
}

pub async fn list(
    State(pool): State<SqlitePool>,
    Query(params): Query<ListParams>,
) -> Result<(HeaderMap, Json<Vec<AgentRow>>), (StatusCode, String)> {
    // #563: validate the status filter up front — a typo'd value
    // silently meaning "all" would defeat the deep link's purpose.
    let status = match params.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s @ ("online" | "offline")) => Some(s.to_string()),
        Some(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "status must be 'online' or 'offline'".to_string(),
            ));
        }
    };

    // #652: text filters are now regexes (matching Activity / Audit).
    // `compile` trims internally, so a whitespace-only box stays "no
    // filter"; an invalid pattern is a 400.
    let q_re = super::compile(params.q.as_deref())?;
    let user_re = super::compile(params.user.as_deref())?;
    let version_re = super::compile(params.version.as_deref())?;
    let has_regex = q_re.is_some() || user_re.is_some() || version_re.is_some();

    let quar_like = quarantined_like(params.quarantined.as_deref());
    // #1051: metadata filter — `(key, Some(like))` requires the key's
    // value to contain `meta_value`; `(key, None)` just requires the key
    // to be present. Blank key → no metadata filter. SQL-expressible, so
    // it rides the fast path and doesn't force the regex scan.
    let meta_filter: Option<(String, Option<String>)> = params
        .meta_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|key| {
            let like = params
                .meta_value
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(contains_like);
            (key.to_string(), like)
        });
    let cutoff = chrono::Utc::now() - ALIVE_THRESHOLD;
    let needs_count = params.limit.is_some();

    if !has_regex {
        // Fast path: every filter is expressible in SQL, so let SQLite
        // do the quarantine pre-filter, the status filter, LIMIT/OFFSET
        // paging, and the fleet-wide count aggregate. Avoids pulling
        // the whole fleet into memory for the common (no-regex) case.
        let limit = params.limit.map(i64::from).unwrap_or(-1);
        let offset = params.offset.map(i64::from).unwrap_or(0);

        let (matched, online): (i64, i64) = if !needs_count {
            (0, 0)
        } else {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                "SELECT COUNT(*) AS matched, CAST(COALESCE(SUM(CASE WHEN \
                 last_heartbeat IS NOT NULL AND last_heartbeat >= ",
            );
            qb.push_bind(cutoff)
                .push(" THEN 1 ELSE 0 END), 0) AS INTEGER) AS online FROM agents");
            let mut started = false;
            push_prefilters(&mut qb, &mut started, &quar_like, &meta_filter);
            let row = qb.build().fetch_one(&pool).await.map_err(|e| {
                warn!(error = %e, "count agents");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "count agents failed".to_string(),
                )
            })?;
            (
                row.try_get("matched").unwrap_or(0),
                row.try_get("online").unwrap_or(0),
            )
        };

        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM agents");
        let mut started = false;
        push_prefilters(&mut qb, &mut started, &quar_like, &meta_filter);
        match status.as_deref() {
            Some("online") => {
                qb.push(sep(&mut started))
                    .push("last_heartbeat IS NOT NULL AND last_heartbeat >= ")
                    .push_bind(cutoff);
            }
            Some("offline") => {
                qb.push(sep(&mut started))
                    .push("(last_heartbeat IS NULL OR last_heartbeat < ")
                    .push_bind(cutoff)
                    .push(")");
            }
            _ => {}
        }
        qb.push(" ORDER BY updated_at DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
            warn!(error = %e, "list agents");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "list agents failed".to_string(),
            )
        })?;
        let mut page: Vec<AgentRow> = rows.into_iter().map(row_to_agent).collect();
        attach_meta(&pool, &mut page).await?;
        let total = total_count(
            needs_count,
            status.as_deref(),
            matched,
            online,
            offset + page.len() as i64,
        );
        return Ok((
            build_headers(needs_count, total, matched, online),
            Json(page),
        ));
    }

    // Regex path: SQL can't run a regex, so pre-filter by quarantine
    // (cheap) and pull the candidate rows, then apply the compiled
    // regexes in Rust. The agents table is one row per PC, so this is
    // a few-thousand-row scan at worst.
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM agents");
    let mut started = false;
    push_prefilters(&mut qb, &mut started, &quar_like, &meta_filter);
    qb.push(" ORDER BY updated_at DESC LIMIT ")
        .push_bind(MAX_FETCH);
    let rows = qb.build().fetch_all(&pool).await.map_err(|e| {
        warn!(error = %e, "list agents");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "list agents failed".to_string(),
        )
    })?;
    if rows.len() as i64 >= MAX_FETCH {
        warn!(
            cap = MAX_FETCH,
            "agents regex prefilter hit the fetch cap; results may be truncated"
        );
    }

    // `matched` = the q/user/version-matching set, BEFORE the status
    // filter, so the per-status chip counts stay fleet-wide.
    let matched_rows: Vec<AgentRow> = rows
        .into_iter()
        .filter(|r| {
            // Match on the raw columns FIRST so a row about to be
            // dropped never pays for row_to_agent's String allocations
            // and the quarantine JSON parse (gemini #661). A NULL TEXT
            // column surfaces as Err on a &str get, collapsed to "" —
            // the same empty-string match semantics results.rs uses.
            if let Some(re) = &q_re {
                let pc: &str = r.try_get("pc_id").unwrap_or("");
                let host: &str = r.try_get("hostname").unwrap_or("");
                if !(re.is_match(pc) || re.is_match(host)) {
                    return false;
                }
            }
            if let Some(re) = &user_re {
                let user: &str = r.try_get("last_logon_user").unwrap_or("");
                let display: &str = r.try_get("last_logon_display_name").unwrap_or("");
                if !(re.is_match(user) || re.is_match(display)) {
                    return false;
                }
            }
            if let Some(re) = &version_re {
                let version: &str = r.try_get("agent_version").unwrap_or("");
                if !re.is_match(version) {
                    return false;
                }
            }
            true
        })
        .map(row_to_agent)
        .collect();

    let matched = matched_rows.len() as i64;
    let online = matched_rows.iter().filter(|a| is_online(a, cutoff)).count() as i64;

    let offset = params.offset.unwrap_or(0) as usize;
    let take = params.limit.map(|n| n as usize).unwrap_or(usize::MAX);
    let mut page: Vec<AgentRow> = matched_rows
        .into_iter()
        .filter(|a| match status.as_deref() {
            Some("online") => is_online(a, cutoff),
            Some("offline") => !is_online(a, cutoff),
            _ => true,
        })
        .skip(offset)
        .take(take)
        .collect();
    attach_meta(&pool, &mut page).await?;

    let total = total_count(
        needs_count,
        status.as_deref(),
        matched,
        online,
        offset as i64 + page.len() as i64,
    );
    Ok((
        build_headers(needs_count, total, matched, online),
        Json(page),
    ))
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

/// `DELETE /api/agents/{pc_id}` — remove an agent from the registry
/// (operator). Audited.
///
/// The `agents` table is a projection of the heartbeat stream, so this is
/// a "drop it from the list now" action, NOT a permanent tombstone: a
/// still-alive agent re-registers (via the heartbeat projector's upsert)
/// on its next heartbeat (~30s). It's therefore only meaningful for
/// genuinely-retired machines — the SPA's confirm dialog says as much.
/// Same self-healing property the config-driven dead-agent prune relies
/// on (#886).
///
/// Idempotent: always returns 204, whether or not a row was present. A
/// repeat delete — or one that races the config-driven auto-prune (#886)
/// having already removed the row — still yields the desired end state
/// (agent absent), so the SPA reports success rather than a spurious
/// "not found" error. The audit event fires only when a row is actually
/// removed, so a no-op delete doesn't litter the trail.
pub async fn delete(
    State(s): State<AppState>,
    caller: Caller,
    Path(pc_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let res = sqlx::query("DELETE FROM agents WHERE pc_id = ?")
        .bind(&pc_id)
        .execute(&s.pool)
        .await
        .map_err(|e| {
            warn!(error = %e, pc_id = %pc_id, "delete agent");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("delete agent {pc_id}: {e}"),
            )
        })?;
    // Only a real deletion is worth an info line + audit row; a no-op
    // (already gone) is a silent success.
    if res.rows_affected() > 0 {
        info!(pc_id = %pc_id, "agent deleted from registry");
        audit::record(
            &s.nats,
            "operator",
            "agent_delete",
            Some(&pc_id),
            Some(&caller),
            serde_json::json!({ "pc_id": pc_id }),
        )
        .await;
    }
    Ok(StatusCode::NO_CONTENT)
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
        // #582 Phase 2: stored as a JSON array TEXT column (NULL =
        // none). A malformed value degrades to empty rather than
        // failing the whole row.
        quarantined_versions: r
            .try_get::<Option<String>, _>("quarantined_versions")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        // #655 follow-up: a local account with no configured display
        // name reports LastLoggedOnDisplayName as an EMPTY STRING (not
        // absent), and older rows may hold a "" the projector stored
        // before this normalisation. Treat "" as None here so the SPA's
        // `display_name || user` fallback shows the login name instead
        // of a blank cell.
        last_logon_user: r
            .try_get::<Option<String>, _>("last_logon_user")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty()),
        last_logon_display_name: r
            .try_get::<Option<String>, _>("last_logon_display_name")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty()),
        // Decorated separately by `attach_meta` after the page is built
        // (the agents row carries no metadata columns of its own).
        meta: Vec::new(),
    }
}

/// `GET /api/agents/meta-keys` — the distinct `agent_meta` keys across the
/// fleet, sorted. Drives the Agents page column picker and the metadata
/// search field's key dropdown (#1051). Viewer-readable (commons), like
/// the rest of the roster substrate.
pub async fn meta_keys(State(pool): State<SqlitePool>) -> Result<Json<Vec<String>>, StatusCode> {
    let rows = sqlx::query("SELECT DISTINCT key FROM agent_meta ORDER BY key")
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "agent meta keys");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(
        rows.into_iter()
            .map(|r| r.try_get::<String, _>("key").unwrap_or_default())
            .collect(),
    ))
}

/// One bucket of the fleet's agent-version histogram. `active` is the
/// subset of `total` whose last heartbeat is within [`ALIVE_THRESHOLD`],
/// so the Dashboard's "version distribution" card reads as rollout
/// coverage (how many of each build are actually live right now), not
/// just an all-time tally that counts long-dead hosts.
#[derive(Serialize)]
pub struct VersionCount {
    /// The reported `agent_version`; `None` for rows that never reported
    /// one (pre-handshake / legacy), surfaced as an "unknown" bucket.
    pub version: Option<String>,
    pub total: i64,
    pub active: i64,
}

/// `GET /api/agents/versions` — agent-version histogram for the whole
/// fleet, busiest build first. The agents table is one row per PC
/// (bounded even at multi-thousand-host scale), so this aggregates in
/// memory rather than leaning on SQLite datetime arithmetic for the
/// liveness cut — the exact same `last_heartbeat >= cutoff` rule the
/// list endpoint applies in Rust, kept consistent on purpose.
pub async fn versions(
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<VersionCount>>, StatusCode> {
    let rows = sqlx::query("SELECT agent_version, last_heartbeat FROM agents")
        .fetch_all(&pool)
        .await
        .map_err(|e| {
            warn!(error = %e, "agent versions");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let cutoff = chrono::Utc::now() - ALIVE_THRESHOLD;
    // Key on the raw version string ("" = unknown) so the bucket is
    // Ord-friendly; map "" back to None on the way out.
    let mut buckets: std::collections::HashMap<String, (i64, i64)> =
        std::collections::HashMap::new();
    for r in rows {
        let version: String = r
            .try_get::<Option<String>, _>("agent_version")
            .ok()
            .flatten()
            .unwrap_or_default();
        let alive = r
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_heartbeat")
            .ok()
            .flatten()
            .is_some_and(|hb| hb >= cutoff);
        let entry = buckets.entry(version).or_insert((0, 0));
        entry.0 += 1;
        if alive {
            entry.1 += 1;
        }
    }
    let mut out: Vec<VersionCount> = buckets
        .into_iter()
        .map(|(version, (total, active))| VersionCount {
            version: (!version.is_empty()).then_some(version),
            total,
            active,
        })
        .collect();
    // Busiest build first; ties broken by version so the order is stable
    // poll-to-poll (a HashMap iterates arbitrarily otherwise).
    out.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then_with(|| a.version.cmp(&b.version))
    });
    Ok(Json(out))
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

    async fn ids_of(pool: SqlitePool, params: ListParams) -> Vec<String> {
        let (_headers, Json(rows)) = list(State(pool), Query(params)).await.unwrap();
        rows.into_iter().map(|r| r.pc_id).collect()
    }

    /// q-only convenience: regex over pc_id / hostname, no paging.
    async fn ids(pool: SqlitePool, q: Option<&str>, limit: Option<u32>) -> Vec<String> {
        ids_of(
            pool,
            ListParams {
                q: q.map(Into::into),
                limit,
                ..Default::default()
            },
        )
        .await
    }

    /// #582 Phase 2: a populated quarantine JSON blob (what the
    /// heartbeat projector writes) round-trips through SQLite and
    /// `row_to_agent`; an absent value and a malformed blob both
    /// degrade to an empty list instead of failing the row.
    #[tokio::test]
    async fn quarantined_versions_decode_through_the_api() {
        let pool = seeded_pool().await;
        sqlx::query("UPDATE agents SET quarantined_versions = ? WHERE pc_id = 'PC001'")
            .bind(r#"["0.43.51","0.43.52"]"#)
            .execute(&pool)
            .await
            .unwrap();
        // A malformed blob must not break the row.
        sqlx::query("UPDATE agents SET quarantined_versions = ? WHERE pc_id = 'PC002'")
            .bind("not json")
            .execute(&pool)
            .await
            .unwrap();

        let (_h, Json(rows)) = list(State(pool), Query(ListParams::default()))
            .await
            .unwrap();
        let by_id = |id: &str| {
            rows.iter()
                .find(|r| r.pc_id == id)
                .unwrap()
                .quarantined_versions
                .clone()
        };
        assert_eq!(by_id("PC001"), vec!["0.43.51", "0.43.52"]);
        assert!(
            by_id("PC002").is_empty(),
            "malformed JSON → empty, not error"
        );
        assert!(by_id("WS-9").is_empty(), "NULL column → empty");
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

    fn get_header(h: &HeaderMap, k: &str) -> i64 {
        h.get(k)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("{k} header missing or unparseable"))
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
                limit: Some(2),
                status: Some("offline".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        // Page is offline-only and capped by limit…
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.pc_id != "PC001"));
        // …while X-Total-Count reflects the active filter (3 offline
        // fleet-wide — paging works past page 1) and the chip counts
        // are fleet-wide regardless of the filter.
        assert_eq!(get_header(&headers, "X-Total-Count"), 3);
        assert_eq!(get_header(&headers, "X-Online-Count"), 1);
        assert_eq!(get_header(&headers, "X-Offline-Count"), 3);
    }

    #[tokio::test]
    async fn online_filter_returns_only_live_agents() {
        let pool = seeded_pool().await;
        set_heartbeat(&pool, "PC001", true).await;
        let (headers, Json(rows)) = list(
            State(pool),
            Query(ListParams {
                limit: Some(10),
                status: Some("online".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.pc_id.as_str()).collect::<Vec<_>>(),
            vec!["PC001"]
        );
        assert_eq!(get_header(&headers, "X-Total-Count"), 1);
    }

    #[tokio::test]
    async fn invalid_status_is_a_bad_request() {
        let pool = seeded_pool().await;
        match list(
            State(pool),
            Query(ListParams {
                limit: Some(10),
                status: Some("onlin".into()),
                ..Default::default()
            }),
        )
        .await
        {
            Err((code, _)) => assert_eq!(code, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("a typo'd status must be a 400, not silently 'all'"),
        }
    }

    #[tokio::test]
    async fn invalid_regex_is_a_bad_request() {
        let pool = seeded_pool().await;
        match list(
            State(pool),
            Query(ListParams {
                q: Some("[unterminated".into()),
                ..Default::default()
            }),
        )
        .await
        {
            Err((code, _)) => assert_eq!(code, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("an invalid regex must be a 400"),
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
                limit: Some(1),
                offset: Some(1),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(
            get_header(&headers, "X-Total-Count"),
            4,
            "seeded fleet has exactly four agents"
        );
    }

    #[tokio::test]
    async fn no_query_returns_whole_fleet() {
        let got = ids(seeded_pool().await, None, None).await;
        assert_eq!(got.len(), 4);
    }

    #[tokio::test]
    async fn blank_query_is_treated_as_no_filter() {
        // Whitespace-only stays "no filter" (trimmed before compile),
        // so it doesn't become a regex that matches a literal space.
        let got = ids(seeded_pool().await, Some("   "), None).await;
        assert_eq!(got.len(), 4);
    }

    #[tokio::test]
    async fn q_is_a_regex_over_pc_id() {
        // Anchored regex — only the two PC0* ids, not WS-9 / web%01.
        let mut got = ids(seeded_pool().await, Some("^PC00"), None).await;
        got.sort();
        assert_eq!(got, vec!["PC001".to_string(), "PC002".to_string()]);
    }

    #[tokio::test]
    async fn q_alternation_matches_pc_id_or_hostname() {
        // `gamma` only exists as a hostname; alternation hits it plus
        // the PC002 id.
        let mut got = ids(seeded_pool().await, Some("PC002|gamma"), None).await;
        got.sort();
        assert_eq!(got, vec!["PC002".to_string(), "WS-9".to_string()]);
    }

    #[tokio::test]
    async fn q_matches_hostname_too() {
        let got = ids(seeded_pool().await, Some("^alpha$"), None).await;
        assert_eq!(got, vec!["PC001".to_string()]);
    }

    #[tokio::test]
    async fn user_regex_matches_either_logon_field() {
        let pool = seeded_pool().await;
        sqlx::query(
            "UPDATE agents SET last_logon_user = ?, last_logon_display_name = ? WHERE pc_id = 'PC001'",
        )
        .bind(r"CORP\taro")
        .bind("Yamada Taro")
        .execute(&pool)
        .await
        .unwrap();
        // Match on the display name…
        let got = ids_of(
            pool.clone(),
            ListParams {
                user: Some("Yamada".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got, vec!["PC001".to_string()]);
        // …and on the login name.
        let got = ids_of(
            pool,
            ListParams {
                user: Some(r"taro".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got, vec!["PC001".to_string()]);
    }

    #[tokio::test]
    async fn version_regex_filters_agent_version() {
        let pool = seeded_pool().await;
        sqlx::query("UPDATE agents SET agent_version = ? WHERE pc_id = 'PC001'")
            .bind("0.43.62")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE agents SET agent_version = ? WHERE pc_id = 'PC002'")
            .bind("0.43.61")
            .execute(&pool)
            .await
            .unwrap();
        let got = ids_of(
            pool,
            ListParams {
                version: Some(r"^0\.43\.62$".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got, vec!["PC001".to_string()]);
    }

    #[tokio::test]
    async fn quarantined_filter_pre_filters_by_version_token() {
        let pool = seeded_pool().await;
        sqlx::query("UPDATE agents SET quarantined_versions = ? WHERE pc_id = 'PC001'")
            .bind(r#"["0.43.62"]"#)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE agents SET quarantined_versions = ? WHERE pc_id = 'PC002'")
            .bind(r#"["0.43.61"]"#)
            .execute(&pool)
            .await
            .unwrap();
        let got = ids_of(
            pool,
            ListParams {
                quarantined: Some("0.43.62".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got, vec!["PC001".to_string()]);
    }

    #[tokio::test]
    async fn quarantined_token_match_is_not_a_substring_match() {
        // `0.43.6` must NOT hit an agent quarantining `0.43.62` —
        // the quoted-token LIKE guards against the substring trap.
        let pool = seeded_pool().await;
        sqlx::query("UPDATE agents SET quarantined_versions = ? WHERE pc_id = 'PC001'")
            .bind(r#"["0.43.62"]"#)
            .execute(&pool)
            .await
            .unwrap();
        let got = ids_of(
            pool,
            ListParams {
                quarantined: Some("0.43.6".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(got.is_empty(), "0.43.6 must not match the 0.43.62 token");
    }

    #[tokio::test]
    async fn quarantined_combines_with_regex_and_counts() {
        // PC001 & PC002 both quarantine 0.43.62; a version regex then
        // narrows to PC001. Counts reflect the combined filter set.
        let pool = seeded_pool().await;
        for pc in ["PC001", "PC002"] {
            sqlx::query("UPDATE agents SET quarantined_versions = ? WHERE pc_id = ?")
                .bind(r#"["0.43.62"]"#)
                .bind(pc)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE agents SET agent_version = ? WHERE pc_id = 'PC001'")
            .bind("0.43.61")
            .execute(&pool)
            .await
            .unwrap();
        let (headers, Json(rows)) = list(
            State(pool),
            Query(ListParams {
                quarantined: Some("0.43.62".into()),
                version: Some("0.43.61".into()),
                limit: Some(10),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.pc_id.as_str()).collect::<Vec<_>>(),
            vec!["PC001"]
        );
        assert_eq!(get_header(&headers, "X-Total-Count"), 1);
    }

    #[tokio::test]
    async fn limit_caps_row_count() {
        let got = ids(seeded_pool().await, None, Some(2)).await;
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn empty_last_logon_display_name_normalises_to_none() {
        // #655 follow-up: a local account reports an empty display
        // name; it must surface as None (not "") so the SPA falls back
        // to the login name instead of rendering a blank cell.
        let pool = seeded_pool().await;
        sqlx::query(
            "UPDATE agents SET last_logon_user = ?, last_logon_display_name = ? WHERE pc_id = 'PC001'",
        )
        .bind(r".\yukimemi")
        .bind("")
        .execute(&pool)
        .await
        .unwrap();
        let (_h, Json(rows)) = list(State(pool), Query(ListParams::default()))
            .await
            .unwrap();
        let a = rows.iter().find(|r| r.pc_id == "PC001").unwrap();
        assert_eq!(a.last_logon_user.as_deref(), Some(r".\yukimemi"));
        assert_eq!(
            a.last_logon_display_name, None,
            "empty display name must normalise to None"
        );
    }

    async fn set_meta(pool: &SqlitePool, pc_id: &str, pairs: &[(&str, &str)]) {
        for (k, v) in pairs {
            sqlx::query("INSERT INTO agent_meta (pc_id, key, value) VALUES (?, ?, ?)")
                .bind(pc_id)
                .bind(k)
                .bind(v)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn meta_is_decorated_onto_rows() {
        let pool = seeded_pool().await;
        set_meta(&pool, "PC001", &[("氏名", "山田"), ("所属", "経理")]).await;
        let (_h, Json(rows)) = list(State(pool), Query(ListParams::default()))
            .await
            .unwrap();
        let pc001 = rows.iter().find(|r| r.pc_id == "PC001").unwrap();
        let map: std::collections::HashMap<_, _> = pc001
            .meta
            .iter()
            .map(|e| (e.key.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(map.get("氏名"), Some(&"山田"));
        assert_eq!(map.get("所属"), Some(&"経理"));
        // A PC with no metadata carries an empty vec (omitted on the wire).
        assert!(
            rows.iter()
                .find(|r| r.pc_id == "PC002")
                .unwrap()
                .meta
                .is_empty()
        );
    }

    #[tokio::test]
    async fn meta_key_filter_requires_the_key() {
        let pool = seeded_pool().await;
        set_meta(&pool, "PC001", &[("所属", "経理")]).await;
        set_meta(&pool, "PC002", &[("氏名", "田中")]).await;
        let got = ids_of(
            pool,
            ListParams {
                meta_key: Some("所属".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got, vec!["PC001".to_string()]);
    }

    #[tokio::test]
    async fn meta_value_is_a_contains_filter() {
        let pool = seeded_pool().await;
        set_meta(&pool, "PC001", &[("氏名", "山田太郎")]).await;
        set_meta(&pool, "PC002", &[("氏名", "田中花子")]).await;
        // Substring match on the value.
        let got = ids_of(
            pool.clone(),
            ListParams {
                meta_key: Some("氏名".into()),
                meta_value: Some("山田".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got, vec!["PC001".to_string()]);
        // A value that matches neither returns nothing.
        let got = ids_of(
            pool,
            ListParams {
                meta_key: Some("氏名".into()),
                meta_value: Some("鈴木".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn meta_filter_counts_are_correct_with_paging() {
        // The metadata filter must flow through the count query so
        // X-Total-Count reflects it, not the whole fleet.
        let pool = seeded_pool().await;
        for pc in ["PC001", "PC002", "WS-9"] {
            set_meta(&pool, pc, &[("dept", "eng")]).await;
        }
        let (headers, Json(rows)) = list(
            State(pool),
            Query(ListParams {
                meta_key: Some("dept".into()),
                meta_value: Some("eng".into()),
                limit: Some(2),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "page capped by limit");
        assert_eq!(
            get_header(&headers, "X-Total-Count"),
            3,
            "count reflects the meta filter"
        );
    }

    #[tokio::test]
    async fn meta_filter_composes_with_regex_path() {
        // A q regex forces the in-Rust regex path; the metadata filter is
        // pre-applied in SQL there too.
        let pool = seeded_pool().await;
        set_meta(&pool, "PC001", &[("dept", "eng")]).await;
        set_meta(&pool, "PC002", &[("dept", "sales")]).await;
        let got = ids_of(
            pool,
            ListParams {
                q: Some("^PC".into()),
                meta_key: Some("dept".into()),
                meta_value: Some("eng".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(got, vec!["PC001".to_string()]);
    }

    #[tokio::test]
    async fn attach_meta_chunks_past_the_bind_parameter_limit() {
        // Regression for the Gemini HIGH: an unbounded list over a fleet
        // larger than META_IN_CHUNK must not trip SQLite's
        // "too many SQL variables". Seed > one chunk of agents and confirm
        // the whole page comes back decorated, error-free.
        let pool = seeded_pool().await;
        let n = META_IN_CHUNK + 50;
        for i in 0..n {
            sqlx::query("INSERT INTO agents (pc_id) VALUES (?)")
                .bind(format!("BULK-{i:04}"))
                .execute(&pool)
                .await
                .unwrap();
        }
        set_meta(&pool, "BULK-0300", &[("dept", "eng")]).await;

        let (_h, Json(rows)) = list(State(pool), Query(ListParams::default()))
            .await
            .unwrap();
        assert!(
            rows.len() >= n,
            "whole fleet returned without a bind-limit error"
        );
        let bulk = rows.iter().find(|r| r.pc_id == "BULK-0300").unwrap();
        assert_eq!(bulk.meta.first().map(|e| e.key.as_str()), Some("dept"));
    }

    #[tokio::test]
    async fn meta_keys_returns_distinct_sorted_keys() {
        let pool = seeded_pool().await;
        set_meta(&pool, "PC001", &[("氏名", "a"), ("所属", "b")]).await;
        set_meta(&pool, "PC002", &[("氏名", "c"), ("メール", "d")]).await;
        let Json(keys) = meta_keys(State(pool)).await.unwrap();
        // Distinct (氏名 once) and sorted by key code point.
        assert_eq!(keys, vec!["メール", "所属", "氏名"]);
    }
}
