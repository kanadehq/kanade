//! v0.31 / #40: derived-table maintenance for inventory `explode`
//! specs. Each inventory manifest can declare one or more
//! [`ExplodeSpec`]s; the projector creates the corresponding flat
//! SQLite table at registration time, then replaces this PC's rows
//! on every result. Cross-PC SQL becomes trivial — `SELECT ... FROM
//! inventory_sw_apps WHERE name LIKE 'Chrome%' AND version < '120'`
//! — without grepping JSON payloads.
//!
//! Identifier safety: operator-supplied table / column names are
//! sanitised by [`validate_ident`] (alpha + underscore + digits
//! only, must start with letter). Anything else aborts the
//! operation with a clean error rather than risking SQL injection
//! through the manifest's YAML.
//!
//! Schema evolution: idempotent. `CREATE TABLE IF NOT EXISTS` +
//! `CREATE INDEX IF NOT EXISTS` so re-registering the same job
//! redoes nothing. Adding a new column to the manifest doesn't
//! auto-`ALTER TABLE` — operators do that manually if they want to
//! reshape an existing table, OR drop+recreate via `kanade
//! inventory reproject` (followup CLI).

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};
use kanade_shared::manifest::{ExplodeColumn, ExplodeSpec, Manifest};
use serde_json::Value as JsonValue;
use sqlx::{AssertSqlSafe, Sqlite, SqlitePool, Transaction};
use tracing::{info, warn};

/// Gemini #85 fix: in-memory cache of derived tables we've already
/// ensured exist + indexed. Hot path is the results projector
/// calling [`ensure_table_cached`] on every inventory ExecResult —
/// pre-cache this was one CREATE TABLE IF NOT EXISTS + N CREATE
/// INDEX IF NOT EXISTS round-trips per result, across every PC
/// reporting data. With the cache, the first delivery per spec
/// pays the DB cost; subsequent deliveries are an in-memory set
/// lookup. Keyed on the spec's table name (operator-unique).
/// Invalidation: process restart re-fills via the startup
/// `ensure_tables_for_jobs` pass — the table itself is idempotent.
fn ensured_tables() -> &'static Mutex<HashSet<String>> {
    static CACHE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Validate that an operator-supplied identifier (table name,
/// column name) is safe to splice into SQL DDL/DML. SQLite has no
/// identifier-parameter binding, so we have to spell out what's
/// acceptable. Conservative: ASCII letters + digits + underscore
/// only, must start with letter, max 64 chars. Anything weirder
/// (quotes, brackets, semicolons, Unicode) aborts.
pub fn validate_ident(ident: &str) -> Result<()> {
    if ident.is_empty() || ident.len() > 64 {
        bail!("identifier {ident:?} must be 1..=64 chars");
    }
    let mut chars = ident.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        bail!("identifier {ident:?} must start with a letter or underscore");
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            bail!("identifier {ident:?} contains invalid character {c:?}");
        }
    }
    Ok(())
}

/// CodeRabbit #85 fix: reject unknown `kind` values instead of
/// silently coercing typos (`kind: int` instead of `integer`) to
/// `TEXT` — which would silently change numeric filter semantics.
/// Empty / `None` continues to mean "default to TEXT" for the
/// common case where operators just omit the field.
fn validate_kind(kind: Option<&str>) -> Result<()> {
    match kind {
        None | Some("text") | Some("integer") | Some("real") => Ok(()),
        Some(other) => {
            bail!("unsupported explode column kind {other:?}; expected text|integer|real")
        }
    }
}

/// Map an operator-supplied `kind:` to a SQLite affinity. Default
/// (`None`) is `TEXT` — most inventory fields are strings (app
/// name, version, file system label). `INTEGER` and `REAL` enable
/// proper numeric ordering on size_bytes etc. Assumes the input
/// was already validated via [`validate_kind`].
fn sql_affinity(kind: Option<&str>) -> &'static str {
    match kind {
        Some("integer") => "INTEGER",
        Some("real") => "REAL",
        _ => "TEXT",
    }
}

/// Build the `CREATE TABLE IF NOT EXISTS` SQL for one spec.
/// Returns the SQL string so the caller can also pass it to logging
/// / dry-run modes. Composite PK = `(pc_id, job_id) +
/// spec.primary_key`. `collected_at` is included so cross-table
/// joins can filter by recency without re-reading `inventory_facts`.
pub fn create_table_sql(spec: &ExplodeSpec) -> Result<String> {
    validate_ident(&spec.table)?;
    if spec.primary_key.is_empty() {
        bail!(
            "explode spec for table {:?} needs at least one primary_key column",
            spec.table,
        );
    }
    // Build the set of declared column names so we can verify each
    // primary_key entry actually corresponds to a real column.
    let column_names: BTreeSet<&str> = spec.columns.iter().map(|c| c.field.as_str()).collect();
    for pk in &spec.primary_key {
        validate_ident(pk)?;
        if !column_names.contains(pk.as_str()) {
            bail!(
                "primary_key entry {pk:?} for table {:?} is not in columns",
                spec.table,
            );
        }
    }

    // Gemini #85 fix: quote every operator-supplied identifier with
    // double quotes so SQL reserved words (`order`, `group`,
    // `index`, `user`, ...) used as column / table names parse
    // cleanly. validate_ident already rejected anything wilder than
    // alpha + underscore + digits, so the quoted form is just
    // "be syntactically safe against the reserved-word list".
    let mut sql = format!("CREATE TABLE IF NOT EXISTS \"{}\" (\n", spec.table);
    sql.push_str("    pc_id TEXT NOT NULL,\n");
    sql.push_str("    job_id TEXT NOT NULL,\n");
    sql.push_str("    collected_at TIMESTAMP,\n");
    for col in &spec.columns {
        validate_ident(&col.field)?;
        validate_kind(col.kind.as_deref())?;
        sql.push_str(&format!(
            "    \"{}\" {},\n",
            col.field,
            sql_affinity(col.kind.as_deref())
        ));
    }
    sql.push_str("    PRIMARY KEY (pc_id, job_id");
    for pk in &spec.primary_key {
        sql.push_str(", \"");
        sql.push_str(pk);
        sql.push('"');
    }
    sql.push_str(")\n);");
    Ok(sql)
}

/// Build all `CREATE INDEX IF NOT EXISTS` statements for columns
/// marked `index: true`. Index name is `idx_<table>_<col>` so a
/// re-registration of the same spec is idempotent.
pub fn create_index_sqls(spec: &ExplodeSpec) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for col in &spec.columns {
        if !col.index {
            continue;
        }
        validate_ident(&col.field)?;
        // Gemini #85 fix: quote identifiers in index DDL too.
        out.push(format!(
            "CREATE INDEX IF NOT EXISTS \"idx_{table}_{col}\" ON \"{table}\"(\"{col}\");",
            table = spec.table,
            col = col.field,
        ));
    }
    Ok(out)
}

/// Create the derived table + indexes for one spec. Called from
/// the projector's startup pass (scan all registered jobs) and from
/// the inventory upsert path (just before writing exploded rows)
/// so a new manifest works without a backend restart.
pub async fn ensure_table(pool: &SqlitePool, spec: &ExplodeSpec) -> Result<()> {
    let table_sql = create_table_sql(spec)?;
    sqlx::query(AssertSqlSafe(table_sql))
        .execute(pool)
        .await
        .map_err(|e| anyhow!("create table {}: {e}", spec.table))?;
    for index_sql in create_index_sqls(spec)? {
        sqlx::query(AssertSqlSafe(index_sql))
            .execute(pool)
            .await
            .map_err(|e| anyhow!("create index for {}: {e}", spec.table))?;
    }
    Ok(())
}

/// Gemini #85 fix: cached version of [`ensure_table`] for the hot
/// per-result path. First call per `spec.table` runs the real
/// `ensure_table`; subsequent calls are a `HashSet` lookup. The
/// startup `ensure_tables_for_jobs` pass already warmed the cache
/// for every registered job, so the hot path is effectively
/// free after backend boot.
pub async fn ensure_table_cached(pool: &SqlitePool, spec: &ExplodeSpec) -> Result<()> {
    {
        let cache = ensured_tables().lock().expect("ensured_tables mutex");
        if cache.contains(&spec.table) {
            return Ok(());
        }
    }
    ensure_table(pool, spec).await?;
    let mut cache = ensured_tables().lock().expect("ensured_tables mutex");
    cache.insert(spec.table.clone());
    Ok(())
}

/// Walk every registered inventory manifest and ensure its derived
/// tables exist. Called once at projector startup. Idempotent.
/// Invalid specs (unknown column types, identifier validation
/// failure) are warn-logged and skipped — one broken manifest
/// shouldn't take the whole backend down.
pub async fn ensure_tables_for_jobs(
    pool: &SqlitePool,
    manifests: impl IntoIterator<Item = Manifest>,
) -> Result<()> {
    for manifest in manifests {
        let Some(inv) = manifest.inventory.as_ref() else {
            continue;
        };
        let Some(specs) = inv.explode.as_ref() else {
            continue;
        };
        for spec in specs {
            // Use the cached variant so the per-result hot path
            // skips this work after boot — the startup pass
            // populates the cache, subsequent results are a
            // HashSet lookup.
            match ensure_table_cached(pool, spec).await {
                Ok(()) => info!(
                    job_id = %manifest.id,
                    table = %spec.table,
                    "explode: derived table ready",
                ),
                Err(e) => warn!(
                    error = %e,
                    job_id = %manifest.id,
                    table = %spec.table,
                    "explode: derived table creation failed (skipped)",
                ),
            }
        }
    }
    Ok(())
}

/// Replace this PC's rows in `spec.table` with the elements of
/// `payload[spec.field]`. Transactional — DELETE-then-INSERT
/// happens atomically so concurrent readers always see a coherent
/// snapshot. A missing / non-array field is treated as an empty
/// array (operator typo / pre-`explode` manifest data / the script
/// omitting the field this run): any prior rows are removed — and,
/// under `track_history`, recorded as `removed` events in the same
/// transaction (#929) — rather than silently skipped.
pub async fn replace_rows(
    pool: &SqlitePool,
    spec: &ExplodeSpec,
    pc_id: &str,
    job_id: &str,
    collected_at: Option<chrono::DateTime<chrono::Utc>>,
    payload: &JsonValue,
) -> Result<usize> {
    validate_ident(&spec.table)?;
    // CodeRabbit #85 fix: defence in depth — validate column
    // identifiers locally even though ensure_table_cached already
    // ran. A future caller that bypasses ensure_table_cached
    // (e.g. tests, a tool calling replace_rows directly) doesn't
    // get to skip identifier-safety checks.
    for col in &spec.columns {
        validate_ident(&col.field)?;
    }
    // A missing / non-array field means "no rows this run" (legacy
    // data, or the script chose to omit the field). #929: treat it as
    // an empty array and flow through the SAME transactional
    // diff → events → delete path below, instead of a bare delete_rows()
    // that wiped the table WITHOUT emitting `removed` history events.
    // That early delete destroyed removal timestamps, so the next run
    // that DID carry the field diffed against an empty table and
    // re-emitted `added` for everything — a phantom "everything just
    // appeared" storm. As an empty array, disappearance is recorded as
    // `removed` and a later reappearance is a legitimate `added` matched
    // by that prior `removed`.
    let arr: &[JsonValue] = payload
        .get(&spec.field)
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // gemini #951: steady-state short-circuit. A PC that *consistently*
    // lacks an optional field would otherwise open a write transaction
    // (SELECT + DELETE + commit — serialized fsync'd I/O) on every scan
    // for nothing. When there's nothing incoming AND no prior rows to
    // remove, there's nothing to do — and no `removed` event to emit —
    // so skip the transaction entirely. The check is a single indexed
    // count on the (pc_id, job_id) PK prefix, and only runs when `arr`
    // is empty (a non-empty payload always has inserts to do).
    if arr.is_empty() {
        let existing: (i64,) = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM \"{}\" WHERE pc_id = ? AND job_id = ?",
            spec.table
        )))
        .bind(pc_id)
        .bind(job_id)
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow!("count prior rows in {}: {e}", spec.table))?;
        if existing.0 == 0 {
            return Ok(0);
        }
    }

    let mut tx: Transaction<'_, Sqlite> = pool.begin().await?;

    // v0.31 / #41: when the spec opts into history tracking, diff
    // the incoming `arr` against the prior rows BEFORE the DELETE
    // wipes them. Events written into the same transaction so a
    // crash mid-replace either commits the full lifecycle
    // (events + new rows) or rolls back to the previous snapshot —
    // history never goes out of sync with the explode table.
    if spec.track_history {
        let events = super::history::diff_explode_rows(&mut tx, spec, pc_id, job_id, arr).await?;
        if !events.is_empty() {
            super::history::write_events(&mut tx, pc_id, job_id, &events).await?;
        }
    }

    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM \"{}\" WHERE pc_id = ? AND job_id = ?",
        spec.table
    )))
    .bind(pc_id)
    .bind(job_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| anyhow!("delete prior rows in {}: {e}", spec.table))?;

    // Gemini #85 fix: quote column identifiers in the INSERT
    // column list too so reserved-word column names work.
    let quoted_columns: Vec<String> = spec
        .columns
        .iter()
        .map(|c| format!("\"{}\"", c.field))
        .collect();
    let placeholders = std::iter::repeat_n("?", spec.columns.len() + 3)
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql = format!(
        "INSERT INTO \"{}\" (pc_id, job_id, collected_at, {}) VALUES ({})",
        spec.table,
        quoted_columns.join(", "),
        placeholders,
    );

    let mut inserted = 0;
    for element in arr {
        let mut q = sqlx::query(AssertSqlSafe(insert_sql.as_str()))
            .bind(pc_id)
            .bind(job_id)
            .bind(collected_at);
        for col in &spec.columns {
            q = bind_column(q, col, element);
        }
        match q.execute(&mut *tx).await {
            Ok(_) => inserted += 1,
            Err(e) => warn!(
                error = %e,
                table = %spec.table,
                pc_id,
                job_id,
                "explode: skip row (insert failed; likely PK collision within payload)",
            ),
        }
    }
    tx.commit().await?;
    Ok(inserted)
}

/// Pull `element[col.field]` and bind it to the query with the
/// right type. JSON null → SQL NULL; missing key → SQL NULL.
fn bind_column<'q>(
    q: sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments>,
    col: &ExplodeColumn,
    element: &'q JsonValue,
) -> sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments> {
    let v = element.get(&col.field);
    match (col.kind.as_deref(), v) {
        (_, None) | (_, Some(JsonValue::Null)) => q.bind(Option::<String>::None),
        (Some("integer"), Some(JsonValue::Number(n))) => q.bind(n.as_i64()),
        (Some("real"), Some(JsonValue::Number(n))) => q.bind(n.as_f64()),
        // Default + text: stringify whatever's there (numbers,
        // bools, strings). Saves operators from having to think
        // about whether `version = "120"` (string) vs 120 (number)
        // in their script's output.
        (_, Some(JsonValue::String(s))) => q.bind(Some(s.clone())),
        (_, Some(JsonValue::Number(n))) => q.bind(Some(n.to_string())),
        (_, Some(JsonValue::Bool(b))) => q.bind(Some(b.to_string())),
        (_, Some(other)) => q.bind(Some(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ident_accepts_normal_names() {
        for ok in ["apps", "inventory_sw_apps", "_underscore", "abc123"] {
            assert!(validate_ident(ok).is_ok(), "{ok} should pass");
        }
    }

    #[test]
    fn validate_ident_rejects_attacks() {
        for bad in [
            "",
            "123leading",
            "with space",
            "drop;",
            "with-dash",
            "apps]",
            "ねこ",
        ] {
            assert!(validate_ident(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    fn sample_apps_spec() -> ExplodeSpec {
        ExplodeSpec {
            field: "apps".into(),
            table: "inventory_sw_apps".into(),
            primary_key: vec!["name".into(), "source".into()],
            track_history: false,
            columns: vec![
                ExplodeColumn {
                    field: "source".into(),
                    kind: Some("text".into()),
                    index: false,
                },
                ExplodeColumn {
                    field: "name".into(),
                    kind: None,
                    index: true,
                },
                ExplodeColumn {
                    field: "version".into(),
                    kind: None,
                    index: false,
                },
                ExplodeColumn {
                    field: "publisher".into(),
                    kind: None,
                    index: false,
                },
            ],
        }
    }

    #[test]
    fn create_table_sql_shape() {
        let sql = create_table_sql(&sample_apps_spec()).unwrap();
        // Gemini #85 fix: identifiers are now double-quoted to
        // survive reserved-word column names (`order`, `group`,
        // etc.). Tests assert against the quoted form.
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS \"inventory_sw_apps\""));
        assert!(sql.contains("pc_id TEXT NOT NULL"));
        assert!(sql.contains("\"name\" TEXT"));
        assert!(sql.contains("PRIMARY KEY (pc_id, job_id, \"name\", \"source\")"));
    }

    #[test]
    fn create_table_sql_rejects_unknown_primary_key() {
        let mut bad = sample_apps_spec();
        bad.primary_key = vec!["nonexistent".into()];
        let err = create_table_sql(&bad).unwrap_err().to_string();
        assert!(err.contains("nonexistent"), "{err}");
    }

    #[test]
    fn create_index_sqls_only_for_marked_columns() {
        let sqls = create_index_sqls(&sample_apps_spec()).unwrap();
        // Only `name` has index: true in sample_apps_spec.
        assert_eq!(sqls.len(), 1);
        // Gemini #85 fix: identifiers double-quoted.
        assert!(sqls[0].contains("\"idx_inventory_sw_apps_name\""));
        assert!(sqls[0].contains("ON \"inventory_sw_apps\"(\"name\")"));
    }

    #[tokio::test]
    async fn ensure_table_and_replace_rows_roundtrip() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let spec = sample_apps_spec();
        ensure_table(&pool, &spec).await.unwrap();

        let payload = serde_json::json!({
            "apps": [
                {"source": "wow6432", "name": "Chrome", "version": "120.0.6099.71", "publisher": "Google"},
                {"source": "x64",      "name": "Chrome", "version": "120.0.6099.71", "publisher": "Google"},
                {"source": "x64",      "name": "Firefox", "version": "122.0", "publisher": "Mozilla"},
            ]
        });
        let n = replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &payload)
            .await
            .unwrap();
        assert_eq!(n, 3);

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inventory_sw_apps")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 3);

        // Replace again with different payload — first PC's rows
        // get DELETEd + new ones INSERTed.
        let payload2 = serde_json::json!({
            "apps": [
                {"source": "x64", "name": "Edge", "version": "121.0", "publisher": "Microsoft"},
            ]
        });
        let n = replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &payload2)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inventory_sw_apps")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1, "old rows replaced, not appended");

        // Cross-PC search exercise.
        let pc2_payload = serde_json::json!({
            "apps": [
                {"source": "x64", "name": "Chrome", "version": "99.0.4844.51", "publisher": "Google"},
            ]
        });
        replace_rows(&pool, &spec, "pc-02", "inventory-sw", None, &pc2_payload)
            .await
            .unwrap();
        let chrome_pcs: Vec<(String, String)> = sqlx::query_as(
            "SELECT pc_id, version FROM inventory_sw_apps WHERE name = ? ORDER BY pc_id",
        )
        .bind("Chrome")
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(chrome_pcs.len(), 1, "pc-01 no longer has Chrome");
        assert_eq!(chrome_pcs[0].0, "pc-02");
        assert_eq!(chrome_pcs[0].1, "99.0.4844.51");
    }

    #[tokio::test]
    async fn replace_rows_with_missing_field_clears_pc_state() {
        // Edge case: manifest declares explode but this PC's
        // payload doesn't have the field (legacy data captured
        // pre-explode, or the script chose to skip). Stale rows
        // from a prior result should still be cleared so the
        // operator doesn't see ghosts.
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let spec = sample_apps_spec();
        ensure_table(&pool, &spec).await.unwrap();

        let prior = serde_json::json!({
            "apps": [{"source": "x64", "name": "Chrome", "version": "120", "publisher": "Google"}]
        });
        replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &prior)
            .await
            .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inventory_sw_apps")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);

        let no_apps_field = serde_json::json!({ "hostname": "pc-01" });
        let n = replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &no_apps_field)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inventory_sw_apps")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "stale rows cleared even when field absent");
    }

    #[tokio::test]
    async fn missing_field_records_removed_not_phantom_added() {
        // #929: with track_history, a transient missing field must record
        // `removed` events (not silently wipe the rows). Before the fix,
        // the missing-field branch deleted rows OUTSIDE the history diff,
        // so the disappearance left no `removed` events and the next run
        // that carried the field again re-emitted `added` for everything
        // — a phantom "everything just appeared" storm.
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // migrations create `inventory_history`; ensure_table the explode table.
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let mut spec = sample_apps_spec();
        spec.track_history = true;
        ensure_table(&pool, &spec).await.unwrap();

        let payload = serde_json::json!({
            "apps": [
                {"source": "x64", "name": "Chrome", "version": "120", "publisher": "Google"},
                {"source": "x64", "name": "Firefox", "version": "122", "publisher": "Mozilla"},
            ]
        });

        // 1) First scan: two apps → two `added`.
        replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &payload)
            .await
            .unwrap();

        // 2) Field missing this run → rows cleared AND two `removed` events.
        let no_field = serde_json::json!({ "hostname": "pc-01" });
        let n = replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &no_field)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let removed: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM inventory_history WHERE change_kind = 'removed'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            removed.0, 2,
            "disappearance must be recorded as `removed`, not a silent wipe (#929)"
        );

        // 3) Field reappears → two more `added`. Every `added` now has a
        //    matching prior `removed`: 4 added total (2 initial + 2
        //    reappearance) against 2 removed — no phantom, the reappearance
        //    is legitimate because the disappearance was recorded.
        replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &payload)
            .await
            .unwrap();
        let added: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM inventory_history WHERE change_kind = 'added'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(added.0, 4);
    }

    #[tokio::test]
    async fn missing_field_on_fresh_pc_is_a_noop() {
        // gemini #951 steady-state short-circuit: a PC that has no prior
        // rows and no field this run does nothing — returns 0 and emits
        // no history events (no spurious `removed` for rows that never
        // existed).
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let mut spec = sample_apps_spec();
        spec.track_history = true;
        ensure_table(&pool, &spec).await.unwrap();

        let no_field = serde_json::json!({ "hostname": "pc-01" });
        let n = replace_rows(&pool, &spec, "pc-01", "inventory-sw", None, &no_field)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inventory_history")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events.0, 0, "no rows existed, so nothing to remove");
    }
}
