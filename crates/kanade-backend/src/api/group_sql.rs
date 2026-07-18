//! #1032: resolve a [`GroupDef`]'s membership — the set of `pc_id`s — with a
//! small in-memory cache for the **dynamic** (SQL) case. Sibling of
//! [`super::view_sql`], which does the same for a `view:` widget.
//!
//! A dynamic group carries a read-only `query:` that returns a `pc_id`
//! column; this module runs it in the shared read-only sandbox
//! ([`super::query`]) and caches the resulting membership on the group's
//! `refresh` cadence, so a scheduler tick that fires several schedules
//! targeting the same group doesn't re-run the correlation each time. A
//! **static** group returns its literal `members:` — no query, no cache.
//!
//! The cache is in-memory and derived (self-healing, no durability needed):
//! keyed by `group.id` with a content hash so an edited query/members
//! recomputes immediately rather than serving a stale set until the cadence
//! elapses. Errors are cached too so a broken query doesn't re-run on every
//! tick — a fixed query changes the hash and recomputes at once.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use kanade_shared::manifest::GroupDef;
use serde_json::Value as JsonValue;
use sqlx::SqlitePool;
use tracing::warn;

use super::AppState;
use super::query::{ReadOnlyError, run_read_only};

/// Row cap for a group query — the maximum members a dynamic group can
/// resolve to. Bound directly to the sandbox's own ceiling
/// ([`super::query::MAX_LIMIT`]) rather than re-declaring the value, so the
/// two can't drift: a runaway cross join is bounded; a fleet is single-digit
/// thousands of PCs in practice, well under this. A group that genuinely
/// exceeds it is truncated (and logged, never silently).
const GROUP_ROW_LIMIT: usize = super::query::MAX_LIMIT;

/// In-memory membership cache — one entry per dynamic group id. `AppState`
/// holds an `Arc` clone so it's shared across requests + the scheduler.
pub type GroupCache = Arc<Mutex<HashMap<String, CacheEntry>>>;

/// A cached membership computation. `hash` fingerprints the group's
/// query+members+refresh so an operator edit (same key, new content) is a miss
/// rather than served stale.
pub struct CacheEntry {
    hash: u64,
    computed_at: Instant,
    result: Result<Vec<String>, String>,
}

/// Fresh, empty cache for `AppState`.
pub fn new_cache() -> GroupCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Fingerprint the parts of a group whose change should invalidate the cache.
fn group_hash(g: &GroupDef) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    g.query.hash(&mut h);
    g.members.hash(&mut h);
    g.refresh.hash(&mut h);
    h.finish()
}

/// Resolve a group's membership. A **static** group returns its literal
/// `members:` verbatim (no query, no cache). A **dynamic** group runs its SQL
/// in the read-only sandbox, serving a fresh-enough cache entry when present
/// and recomputing on a miss / stale / edited entry.
///
/// The returned `pc_id`s are the raw query output — callers (the scheduler's
/// `resolve_roster`) intersect them with the live `agents` roster, so an id
/// the query invents that isn't a real agent simply drops out there.
pub async fn resolve_group_members(
    state: &AppState,
    group: &GroupDef,
) -> Result<Vec<String>, String> {
    let Some(query) = group.dynamic_query() else {
        // Static group: literal membership, nothing to run or cache.
        return Ok(group.members.clone());
    };

    let hash = group_hash(group);
    let ttl = group.refresh_interval();
    {
        let cache = state.group_cache.lock().expect("group_cache mutex");
        if let Some(e) = cache.get(&group.id)
            && e.hash == hash
            && e.computed_at.elapsed() < ttl
        {
            return e.result.clone();
        }
    }
    // Recompute outside the lock (the query awaits). A concurrent duplicate
    // recompute is harmless — both write the same key, last wins.
    let result = run_group_query(&state.query_pool, &group.id, query).await;
    let mut cache = state.group_cache.lock().expect("group_cache mutex");
    cache.insert(
        group.id.clone(),
        CacheEntry {
            hash,
            computed_at: Instant::now(),
            result: result.clone(),
        },
    );
    result
}

/// Run a dynamic group's query and pull the `pc_id` column. Kept separate from
/// the cache so it's unit-testable with a bare pool. The query's contract is a
/// single required `pc_id` column (extra columns are allowed — handy for
/// debugging the query in isolation — but ignored here).
async fn run_group_query(pool: &SqlitePool, id: &str, query: &str) -> Result<Vec<String>, String> {
    let res = run_read_only(pool, query, GROUP_ROW_LIMIT, None)
        .await
        .map_err(|e: ReadOnlyError| e.to_string())?;

    // SQL column names are case-insensitive, so match the `pc_id` contract
    // column regardless of the casing the operator's query/alias produced
    // (`SELECT pc_id`, `... AS PC_ID`, …).
    let idx = res
        .columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case("pc_id"))
        .ok_or_else(|| {
            format!(
                "group query must return a `pc_id` column (returned: {})",
                res.columns.join(", ")
            )
        })?;

    if res.truncated {
        // Never silently cap membership — a truncated group would quietly drop
        // hosts from a schedule's target.
        warn!(
            group = %id,
            cap = GROUP_ROW_LIMIT,
            "group query hit the row cap — membership truncated; tighten the query",
        );
    }

    let mut out = Vec::with_capacity(res.rows.len());
    for row in &res.rows {
        match row.get(idx) {
            Some(JsonValue::String(s)) => out.push(s.clone()),
            // A NULL pc_id can't identify a host — drop it rather than emit an
            // empty-string member.
            Some(JsonValue::Null) | None => {}
            // pc_id is a TEXT column in practice; defend against a numeric one
            // by stringifying rather than dropping it.
            Some(other) => out.push(other.to_string()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool_with_agents() -> SqlitePool {
        // `sqlite::memory:` is connection-local — pin to one connection so
        // setup + query hit the same DB (matches view_sql's tests).
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE agents (pc_id TEXT, hostname TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agents VALUES ('SRV-1','SRV-1'), ('PC-1','PC-1'), ('SRV-2','SRV-2')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn dynamic_query_extracts_pc_id_column() {
        let pool = pool_with_agents().await;
        let ids = run_group_query(
            &pool,
            "servers",
            "SELECT pc_id FROM agents WHERE hostname LIKE 'SRV-%' ORDER BY pc_id",
        )
        .await
        .unwrap();
        assert_eq!(ids, vec!["SRV-1", "SRV-2"]);
    }

    #[tokio::test]
    async fn extra_columns_are_ignored_pc_id_is_found_by_name() {
        let pool = pool_with_agents().await;
        // pc_id not first, plus a debug column — still resolved by name.
        let ids = run_group_query(
            &pool,
            "g",
            "SELECT hostname, pc_id FROM agents WHERE pc_id = 'PC-1'",
        )
        .await
        .unwrap();
        assert_eq!(ids, vec!["PC-1"]);
    }

    #[tokio::test]
    async fn pc_id_column_match_is_case_insensitive() {
        let pool = pool_with_agents().await;
        // Operator aliased the column to a different casing — still resolved.
        let ids = run_group_query(
            &pool,
            "g",
            "SELECT pc_id AS PC_ID FROM agents WHERE hostname = 'PC-1'",
        )
        .await
        .unwrap();
        assert_eq!(ids, vec!["PC-1"]);
    }

    #[tokio::test]
    async fn missing_pc_id_column_is_an_error() {
        let pool = pool_with_agents().await;
        let err = run_group_query(&pool, "g", "SELECT hostname FROM agents")
            .await
            .unwrap_err();
        assert!(err.contains("pc_id"), "err: {err}");
    }

    #[tokio::test]
    async fn write_query_is_rejected_read_only() {
        let pool = pool_with_agents().await;
        let err = run_group_query(&pool, "g", "DELETE FROM agents")
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("read-only"), "err: {err}");
    }

    #[tokio::test]
    async fn null_pc_id_rows_are_dropped() {
        let pool = pool_with_agents().await;
        sqlx::query("INSERT INTO agents VALUES (NULL, 'ghost')")
            .execute(&pool)
            .await
            .unwrap();
        let ids = run_group_query(
            &pool,
            "g",
            "SELECT pc_id FROM agents WHERE hostname = 'ghost'",
        )
        .await
        .unwrap();
        assert!(ids.is_empty(), "null pc_id dropped: {ids:?}");
    }
}
