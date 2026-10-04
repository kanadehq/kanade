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
//! Schema evolution (#1492): [`ensure_table`] reconciles the derived
//! table's actual schema against the spec's expected schema every
//! time it runs, not just on first creation. A missing table is
//! created via `CREATE TABLE IF NOT EXISTS` as before. An existing
//! table whose only drift is new columns gets `ALTER TABLE ADD
//! COLUMN` for each addition. Anything that can't be expressed as a
//! pure addition — a `primary_key` change, a column type change, a
//! removed column — triggers a rebuild: a new table is created under
//! a fresh, per-call temp name (never derived from `spec.table`, so
//! it can never collide with another job's real explode table — see
//! [`migration_temp_table_name`]) with the current spec's schema,
//! common columns are copied over with `INSERT OR IGNORE` (rows
//! whose old identity collides under the new primary key are
//! dropped rather than guessed at), the old table is dropped, and
//! the temp table is renamed into place. Before this fix,
//! `ensure_table` was `CREATE TABLE IF NOT EXISTS` only: a manifest
//! edit to `primary_key` / `columns` left the on-disk table exactly
//! as it was created the first time, and every subsequent
//! `replace_rows` call silently succeeded at the top-level
//! `inventory_facts` layer while inserting into a table whose schema
//! (and PK) didn't match the payload it was building rows from, so
//! future inserts either errored per-row (logged as a debug-only
//! warn and swallowed by `replace_rows`' per-row error handling) or
//! landed in stale columns — either way the operator never saw an
//! error anywhere in `job create` / `job validate` / `exec` output.
//!
//! Atomicity across a whole manifest: [`ensure_table`] and
//! [`ensure_tables_atomic`] each run their DDL inside exactly one
//! transaction — the former for a single spec, the latter across
//! every `inventory.explode` spec in a manifest. `job create`'s HTTP
//! handler uses [`ensure_tables_atomic`] specifically so a manifest
//! declaring several explode specs either migrates ALL of them or
//! NONE: if spec N+1 fails, spec N's already-executed rebuild rolls
//! back with it instead of leaving a derived table already mutated
//! (with real row loss) while the request reports failure and the
//! stored manifest stays on the old version.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};
use kanade_shared::manifest::{ExplodeColumn, ExplodeSpec, Manifest};
use serde_json::Value as JsonValue;
use sqlx::{AssertSqlSafe, Row, Sqlite, SqliteConnection, SqlitePool, Transaction};
use tracing::{error, info, warn};

/// #1492 fix: in-memory cache of derived tables we've already
/// reconciled against their current spec. Hot path is the results
/// projector calling [`ensure_table_cached`] on every inventory
/// ExecResult — pre-cache this was one CREATE TABLE IF NOT EXISTS + N
/// CREATE INDEX IF NOT EXISTS round-trips per result, across every PC
/// reporting data. With the cache, the first delivery per spec pays
/// the DB cost; subsequent deliveries are an in-memory map lookup.
///
/// Keyed on the spec's table name, valued on the spec's expected
/// `CREATE TABLE` DDL (see [`create_table_sql`]) — NOT just "have we
/// ever seen this table". A manifest edit that changes `primary_key`
/// or `columns` produces a different DDL string for the same table
/// name, so the cache misses and [`ensure_table`] re-runs its
/// reconcile pass instead of trusting a stale "already ensured"
/// marker. Before this fix the cache was a bare `HashSet<String>`
/// keyed on table name only, which meant a schema change made via
/// `kanade job create` was invisible to every already-warm process —
/// the derived table silently kept its original schema forever, even
/// though `ensure_table` itself now knows how to migrate it.
fn ensured_tables() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
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
    // SQLite column names are case-insensitive; a duplicate or a clash
    // with a built-in column would only fail at CREATE/ALTER time, i.e.
    // after `job create` already stored the manifest.
    let mut seen: BTreeSet<String> = ["pc_id", "job_id", "collected_at"]
        .into_iter()
        .map(String::from)
        .collect();
    for col in &spec.columns {
        validate_ident(&col.field)?;
        validate_kind(col.kind.as_deref())?;
        if !seen.insert(col.field.to_ascii_lowercase()) {
            bail!(
                "column {:?} for table {:?} is declared twice or collides with a built-in \
                 column (pc_id, job_id, collected_at)",
                col.field,
                spec.table,
            );
        }
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

/// #1492: what [`ensure_table`] actually did to reconcile a derived
/// table against its spec, so callers (`job create`'s HTTP handler
/// in particular) can report it instead of swallowing it. All-zero /
/// `rebuilt: false` means the table either didn't exist yet (plain
/// create) or already matched the spec exactly (no-op).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SchemaChange {
    /// Columns added via `ALTER TABLE ADD COLUMN` (additive path).
    pub added_columns: Vec<String>,
    /// `true` if the table was dropped and recreated under the new
    /// schema (primary_key change, column removal, or a column type
    /// change — none of which a plain `ALTER TABLE ADD COLUMN` can
    /// express).
    pub rebuilt: bool,
    /// Rows successfully copied from the old table into the rebuilt
    /// one. Only meaningful when `rebuilt` is true.
    pub rows_copied: i64,
    /// Rows that existed in the old table but were NOT copied,
    /// because they collided with another row under the new primary
    /// key (`INSERT OR IGNORE` dropped them) or referenced no
    /// surviving column. Only meaningful when `rebuilt` is true — a
    /// non-zero value here means the migration is lossy and the
    /// operator should expect those PCs' rows to reappear only after
    /// their next `exec`.
    pub rows_lost: i64,
}

impl SchemaChange {
    /// Whether this change is worth telling the operator about — a
    /// pure create-or-noop isn't.
    pub fn is_notable(&self) -> bool {
        self.rebuilt || !self.added_columns.is_empty()
    }
}

/// One column as reported by `PRAGMA table_info`.
struct ExistingColumn {
    name: String,
    /// Declared type as SQLite stored it (`TEXT` / `INTEGER` / `REAL`
    /// / ...), uppercased for comparison against [`sql_affinity`].
    decl_type: String,
    /// 1-based position within the primary key, 0 if not part of it.
    pk_seq: i64,
}

/// Read `table`'s current schema via `PRAGMA table_info`. Returns
/// `None` if the table doesn't exist yet. `table` must already be
/// validated by the caller — every call site routes through either
/// [`create_table_sql`] or [`migration_temp_table_name`] first, both
/// of which validate it.
async fn existing_columns(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<Option<Vec<ExistingColumn>>> {
    let exists: Option<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| anyhow!("check existence of table {table}: {e}"))?;
    if exists.is_none() {
        return Ok(None);
    }

    let rows = sqlx::query(AssertSqlSafe(format!("PRAGMA table_info(\"{table}\")")))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| anyhow!("read table_info for {table}: {e}"))?;
    let cols = rows
        .into_iter()
        .map(|r| ExistingColumn {
            name: r.get::<String, _>("name"),
            decl_type: r.get::<String, _>("type").to_ascii_uppercase(),
            pk_seq: r.get::<i64, _>("pk"),
        })
        .collect();
    Ok(Some(cols))
}

/// The columns + affinities a spec expects, in declaration order,
/// including the three built-ins every explode table carries.
fn expected_columns(spec: &ExplodeSpec) -> Vec<(String, &'static str)> {
    let mut out = vec![
        ("pc_id".to_string(), "TEXT"),
        ("job_id".to_string(), "TEXT"),
        ("collected_at".to_string(), "TIMESTAMP"),
    ];
    for col in &spec.columns {
        out.push((col.field.clone(), sql_affinity(col.kind.as_deref())));
    }
    out
}

/// Reconcile an existing table against `spec`'s expected schema.
/// Additive-only drift (new columns, everything else identical) is
/// migrated with `ALTER TABLE ADD COLUMN`. Anything else — a
/// `primary_key` change, a removed column, or a type change on a
/// surviving column — triggers [`rebuild_table`].
async fn reconcile_existing_table(
    conn: &mut SqliteConnection,
    spec: &ExplodeSpec,
    existing: &[ExistingColumn],
) -> Result<SchemaChange> {
    let expected = expected_columns(spec);
    let expected_names: BTreeSet<&str> = expected.iter().map(|(n, _)| n.as_str()).collect();
    let existing_by_name: HashMap<&str, &ExistingColumn> =
        existing.iter().map(|c| (c.name.as_str(), c)).collect();

    let missing: Vec<(String, &'static str)> = expected
        .iter()
        .filter(|(name, _)| !existing_by_name.contains_key(name.as_str()))
        .cloned()
        .collect();
    let has_extra_columns = existing
        .iter()
        .any(|c| !expected_names.contains(c.name.as_str()));
    let has_type_drift = expected.iter().any(|(name, affinity)| {
        existing_by_name
            .get(name.as_str())
            .is_some_and(|c| c.decl_type != *affinity)
    });

    let expected_pk: BTreeSet<&str> = std::iter::empty()
        .chain(["pc_id", "job_id"])
        .chain(spec.primary_key.iter().map(String::as_str))
        .collect();
    let existing_pk: BTreeSet<&str> = existing
        .iter()
        .filter(|c| c.pk_seq > 0)
        .map(|c| c.name.as_str())
        .collect();
    let pk_changed = expected_pk != existing_pk;

    if pk_changed || has_extra_columns || has_type_drift {
        return rebuild_table(conn, spec, existing).await;
    }

    for (name, affinity) in &missing {
        let sql = format!(
            "ALTER TABLE \"{}\" ADD COLUMN \"{name}\" {affinity}",
            spec.table
        );
        sqlx::query(AssertSqlSafe(sql))
            .execute(&mut *conn)
            .await
            .map_err(|e| anyhow!("add column {name} to {}: {e}", spec.table))?;
    }
    Ok(SchemaChange {
        added_columns: missing.into_iter().map(|(n, _)| n).collect(),
        ..Default::default()
    })
}

/// #1492 review fix (R1-1-1): a fresh, globally-unpredictable temp
/// table name for every [`rebuild_table`] call. The original
/// implementation derived the temp name deterministically from
/// `spec.table` (`"{spec.table}__migrate"`) and unconditionally
/// `DROP TABLE IF EXISTS`-ed it first. If any OTHER job's
/// `explode.table` ever happened to equal that exact string — an
/// operator naming their own table `"foo__migrate"`, coincidentally
/// or deliberately once this suffix convention became known — a
/// `primary_key` change to `foo` would drop that unrelated table and
/// its data outright, then rename the rebuild's temp table over it.
/// Naming the temp table from process-local time + a monotonic
/// counter instead means it carries no relationship to any
/// operator-chosen name, so it cannot coincide with another job's
/// real table, and [`rebuild_table`] additionally refuses to proceed
/// (rather than dropping or reusing) if the generated name somehow
/// already exists.
fn migration_temp_table_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("_explode_migrate_{nanos:x}_{seq:x}")
}

/// Rebuild `spec.table` under its current schema: create a
/// same-shape table under a fresh temp name (see
/// [`migration_temp_table_name`]), copy over every column the old
/// and new schemas share via `INSERT OR IGNORE` (rows that collide
/// under the new primary key, or that have no surviving column at
/// all, are dropped rather than guessed at — recovered on this PC's
/// next `exec`, which does a full replace), then swap the temp table
/// in for the original. Runs on whatever connection the caller
/// passes in — [`ensure_table`] / [`ensure_tables_atomic`] wrap the
/// whole reconcile (this included) in one transaction, so a
/// concurrent reader never observes a dropped-but-not-yet-recreated
/// table, and a later step failing (e.g. a sibling spec in the same
/// manifest) rolls this rebuild back too.
async fn rebuild_table(
    conn: &mut SqliteConnection,
    spec: &ExplodeSpec,
    existing: &[ExistingColumn],
) -> Result<SchemaChange> {
    let expected = expected_columns(spec);
    let existing_names: BTreeSet<&str> = existing.iter().map(|c| c.name.as_str()).collect();
    let missing: Vec<String> = expected
        .iter()
        .filter(|(n, _)| !existing_names.contains(n.as_str()))
        .map(|(n, _)| n.clone())
        .collect();
    let common_columns: Vec<&str> = expected
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| existing_names.contains(n))
        .collect();

    let tmp_table = migration_temp_table_name();
    validate_ident(&tmp_table)?;
    if existing_columns(&mut *conn, &tmp_table).await?.is_some() {
        // Astronomically unlikely (would require another in-flight
        // migration to land on the exact same nanosecond + counter
        // value) — fail loudly rather than dropping or reusing a
        // table we can't be sure is ours.
        bail!(
            "migration temp table {tmp_table} unexpectedly already exists; \
             aborting this migration rather than touching it"
        );
    }
    let mut tmp_spec = spec.clone();
    tmp_spec.table = tmp_table.clone();
    let create_tmp_sql = create_table_sql(&tmp_spec)?;

    sqlx::query(AssertSqlSafe(create_tmp_sql))
        .execute(&mut *conn)
        .await
        .map_err(|e| anyhow!("create migration temp table {tmp_table}: {e}"))?;

    let before: (i64,) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM \"{}\"",
        spec.table
    )))
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| anyhow!("count rows in {}: {e}", spec.table))?;

    let rows_copied = if common_columns.is_empty() {
        0
    } else {
        let col_list = common_columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let copy_sql = format!(
            "INSERT OR IGNORE INTO \"{tmp_table}\" ({col_list}) SELECT {col_list} FROM \"{}\"",
            spec.table,
        );
        sqlx::query(AssertSqlSafe(copy_sql))
            .execute(&mut *conn)
            .await
            .map_err(|e| anyhow!("copy rows into migration temp table {tmp_table}: {e}"))?
            .rows_affected() as i64
    };

    sqlx::query(AssertSqlSafe(format!("DROP TABLE \"{}\"", spec.table)))
        .execute(&mut *conn)
        .await
        .map_err(|e| anyhow!("drop old table {}: {e}", spec.table))?;
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE \"{tmp_table}\" RENAME TO \"{}\"",
        spec.table
    )))
    .execute(&mut *conn)
    .await
    .map_err(|e| anyhow!("rename migration temp table into {}: {e}", spec.table))?;

    let rows_lost = (before.0 - rows_copied).max(0);
    if rows_lost > 0 {
        warn!(
            table = %spec.table,
            rows_lost,
            "explode: schema rebuild dropped rows that collided under the new primary key; \
             they will reappear after their PC's next exec",
        );
    }
    Ok(SchemaChange {
        added_columns: missing,
        rebuilt: true,
        rows_copied,
        rows_lost,
    })
}

/// Reconcile the derived table + indexes for one spec against
/// whatever connection the caller passes in. Pure logic — no
/// transaction handling here; [`ensure_table`] and
/// [`ensure_tables_atomic`] own that, so a batch of specs migrated
/// together can share a single all-or-nothing transaction.
async fn reconcile_table(conn: &mut SqliteConnection, spec: &ExplodeSpec) -> Result<SchemaChange> {
    // Also validates every identifier in the spec.
    let table_sql = create_table_sql(spec)?;
    let change = match existing_columns(&mut *conn, &spec.table).await? {
        None => {
            sqlx::query(AssertSqlSafe(table_sql))
                .execute(&mut *conn)
                .await
                .map_err(|e| anyhow!("create table {}: {e}", spec.table))?;
            SchemaChange::default()
        }
        Some(cols) => reconcile_existing_table(conn, spec, &cols).await?,
    };
    for index_sql in create_index_sqls(spec)? {
        sqlx::query(AssertSqlSafe(index_sql))
            .execute(&mut *conn)
            .await
            .map_err(|e| anyhow!("create index for {}: {e}", spec.table))?;
    }
    Ok(change)
}

/// Pure (no DB) validation of every spec's identifiers, primary key
/// and column kinds. `job create` runs this BEFORE writing the
/// catalog so the later SQL migration can only fail on I/O, not on a
/// bad spec the catalog already accepted.
pub fn validate_specs(specs: &[ExplodeSpec]) -> Result<()> {
    for spec in specs {
        create_table_sql(spec)?;
        create_index_sqls(spec)?;
    }
    Ok(())
}

/// Create (or migrate) the derived table + indexes for one spec,
/// inside its own transaction. Called from the projector's startup
/// pass (scan all registered jobs) and from the inventory upsert
/// path (just before writing exploded rows), so a new or changed
/// manifest works without a backend restart. `job create`'s HTTP
/// handler uses [`ensure_tables_atomic`] instead, so a manifest with
/// several explode specs migrates them as one unit — see the module
/// doc for why. See also the module doc for the reconcile algorithm
/// (#1492).
pub async fn ensure_table(pool: &SqlitePool, spec: &ExplodeSpec) -> Result<SchemaChange> {
    let mut tx: Transaction<'_, Sqlite> = pool.begin().await?;
    let change = reconcile_table(&mut tx, spec).await?;
    tx.commit().await?;
    Ok(change)
}

/// Reconcile every spec in `specs` inside ONE transaction. `job
/// create`'s HTTP handler calls this (instead of [`ensure_table`]
/// once per spec) so a manifest declaring multiple
/// `inventory.explode` entries either migrates ALL of them or NONE
/// (review R1-2-1): if spec N+1's reconcile fails, the transaction
/// is dropped without a commit, which rolls back every earlier
/// spec's ALTER/rebuild from this same call too — the DB ends up
/// exactly where it started, matching the unchanged catalog the
/// failed request also left behind. Returns the specs' changes in
/// the same order as `specs`.
pub async fn ensure_tables_atomic(
    pool: &SqlitePool,
    specs: &[ExplodeSpec],
) -> Result<Vec<SchemaChange>> {
    let mut tx: Transaction<'_, Sqlite> = pool.begin().await?;
    let mut changes = Vec::with_capacity(specs.len());
    for spec in specs {
        changes.push(reconcile_table(&mut tx, spec).await?);
    }
    tx.commit().await?;
    Ok(changes)
}

/// Cached version of [`ensure_table`] for the hot per-result path.
/// The cache key is `spec.table`; the cache VALUE is the spec's
/// expected `CREATE TABLE` DDL, so a manifest edit that changes
/// `primary_key` / `columns` invalidates the cache entry for that
/// table (see [`ensured_tables`]) and drives a real reconcile pass
/// instead of trusting a stale "already ensured" marker. The startup
/// `ensure_tables_for_jobs` pass already warms the cache for every
/// registered job, so the hot path is effectively free after backend
/// boot — until the next manifest edit for that table.
pub async fn ensure_table_cached(pool: &SqlitePool, spec: &ExplodeSpec) -> Result<SchemaChange> {
    let expected_sql = create_table_sql(spec)?;
    {
        let cache = ensured_tables().lock().expect("ensured_tables mutex");
        if cache.get(&spec.table) == Some(&expected_sql) {
            return Ok(SchemaChange::default());
        }
    }
    let change = ensure_table(pool, spec).await?;
    let mut cache = ensured_tables().lock().expect("ensured_tables mutex");
    cache.insert(spec.table.clone(), expected_sql);
    Ok(change)
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
                Ok(change) if change.is_notable() => info!(
                    job_id = %manifest.id,
                    table = %spec.table,
                    rebuilt = change.rebuilt,
                    added_columns = ?change.added_columns,
                    rows_lost = change.rows_lost,
                    "explode: derived table schema migrated at startup",
                ),
                Ok(_) => info!(
                    job_id = %manifest.id,
                    table = %spec.table,
                    "explode: derived table ready",
                ),
                Err(e) => error!(
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

    fn items_spec_v1() -> ExplodeSpec {
        ExplodeSpec {
            field: "items".into(),
            table: "example_items".into(),
            primary_key: vec!["item_id".into()],
            track_history: false,
            columns: vec![
                ExplodeColumn {
                    field: "item_id".into(),
                    kind: Some("text".into()),
                    index: false,
                },
                ExplodeColumn {
                    field: "name".into(),
                    kind: Some("text".into()),
                    index: false,
                },
            ],
        }
    }

    /// #1492 repro: primary_key changes from `item_id` to `name`, and a
    /// new `kind` column is added.
    fn items_spec_v2() -> ExplodeSpec {
        ExplodeSpec {
            field: "items".into(),
            table: "example_items".into(),
            primary_key: vec!["name".into()],
            track_history: false,
            columns: vec![
                ExplodeColumn {
                    field: "item_id".into(),
                    kind: Some("text".into()),
                    index: false,
                },
                ExplodeColumn {
                    field: "name".into(),
                    kind: Some("text".into()),
                    index: false,
                },
                ExplodeColumn {
                    field: "kind".into(),
                    kind: Some("text".into()),
                    index: false,
                },
            ],
        }
    }

    /// #1492 repro, end to end at the reconcile layer: v1 spec creates
    /// the table and gets one row written by `exec`. The manifest is
    /// then edited exactly as in the issue — `primary_key` changes and
    /// a column is added — and `ensure_table` (what `job create` now
    /// calls) must migrate the on-disk table so the SAME PC's next
    /// `exec` actually lands a row with the new column populated,
    /// instead of the table silently freezing at its original schema.
    #[tokio::test]
    async fn ensure_table_migrates_primary_key_and_new_column() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // 1) Register + exec against the original schema.
        let v1 = items_spec_v1();
        ensure_table(&pool, &v1).await.unwrap();
        let payload_v1 = serde_json::json!({
            "items": [{"item_id": "i-1", "name": "Widget"}]
        });
        let n = replace_rows(&pool, &v1, "pc-01", "job-items", None, &payload_v1)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // 2) Manifest edited: primary_key item_id -> name, `kind` added.
        //    `job create` now runs `ensure_table` again with the new
        //    spec — this must not be a silent CREATE TABLE IF NOT
        //    EXISTS no-op.
        let v2 = items_spec_v2();
        let change = ensure_table(&pool, &v2).await.unwrap();
        assert!(change.rebuilt, "primary_key change must trigger a rebuild");
        assert_eq!(
            change.rows_copied, 1,
            "the existing row survives the rebuild"
        );
        assert_eq!(change.rows_lost, 0);

        let ddl: (String,) = sqlx::query_as(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'example_items'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            ddl.0.contains("PRIMARY KEY (pc_id, job_id, \"name\")"),
            "on-disk schema must reflect the new primary_key: {}",
            ddl.0
        );
        assert!(
            ddl.0.contains("\"kind\" TEXT"),
            "on-disk schema must carry the new column: {}",
            ddl.0
        );

        // 3) The same PC execs again under the new schema — this is
        //    the step that silently dropped rows before the fix: the
        //    table's actual PRIMARY KEY still didn't include `name`,
        //    so this insert either errored per-row or wrote into a
        //    stale layout. It must now succeed and be readable back.
        let payload_v2 = serde_json::json!({
            "items": [{"item_id": "i-1", "name": "Widget", "kind": "hardware"}]
        });
        let n = replace_rows(&pool, &v2, "pc-01", "job-items", None, &payload_v2)
            .await
            .unwrap();
        assert_eq!(n, 1, "post-migration exec must actually insert a row");

        let row: (String, String, String) =
            sqlx::query_as("SELECT item_id, name, kind FROM example_items WHERE pc_id = 'pc-01'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            row,
            (
                "i-1".to_string(),
                "Widget".to_string(),
                "hardware".to_string()
            )
        );
    }

    /// Additive-only drift (new column, same primary_key) must use
    /// `ALTER TABLE ADD COLUMN` rather than a rebuild — cheaper, and a
    /// useful contrast against the rebuild path above.
    #[tokio::test]
    async fn ensure_table_adds_column_without_rebuild_when_pk_unchanged() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        let v1 = items_spec_v1();
        ensure_table(&pool, &v1).await.unwrap();
        replace_rows(
            &pool,
            &v1,
            "pc-01",
            "job-items",
            None,
            &serde_json::json!({"items": [{"item_id": "i-1", "name": "Widget"}]}),
        )
        .await
        .unwrap();

        let mut v1_plus_kind = v1.clone();
        v1_plus_kind.columns.push(ExplodeColumn {
            field: "kind".into(),
            kind: Some("text".into()),
            index: false,
        });
        let change = ensure_table(&pool, &v1_plus_kind).await.unwrap();
        assert!(
            !change.rebuilt,
            "same primary_key + additive column must not rebuild"
        );
        assert_eq!(change.added_columns, vec!["kind".to_string()]);

        // The pre-existing row survives untouched (rebuild would also
        // preserve it, but the point here is that ALTER TABLE never
        // touches existing rows at all).
        let row: (String, String, Option<String>) =
            sqlx::query_as("SELECT item_id, name, kind FROM example_items WHERE pc_id = 'pc-01'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "i-1");
        assert_eq!(row.1, "Widget");
        assert_eq!(row.2, None);
    }

    /// A re-registration with an unchanged spec must be a true no-op —
    /// no `ALTER TABLE`, no rebuild — matching the module's pre-#1492
    /// idempotence guarantee.
    #[tokio::test]
    async fn ensure_table_noop_when_spec_unchanged() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let spec = items_spec_v1();
        ensure_table(&pool, &spec).await.unwrap();
        let change = ensure_table(&pool, &spec).await.unwrap();
        assert!(!change.rebuilt);
        assert!(change.added_columns.is_empty());
    }

    /// #1492: the per-result hot-path cache must key on the spec's
    /// shape, not just the table name — otherwise a process that had
    /// already warmed the cache for `example_items` under the v1 spec
    /// would treat the v2 spec's arrival as "nothing to do" forever,
    /// even though `ensure_table` itself now knows how to migrate it.
    #[tokio::test]
    async fn ensure_table_cached_detects_spec_change_and_reconciles() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        let v1 = items_spec_v1();
        ensure_table_cached(&pool, &v1).await.unwrap();
        // Cache hit: cheap no-op re-ensure of the same spec.
        let noop = ensure_table_cached(&pool, &v1).await.unwrap();
        assert!(!noop.rebuilt && noop.added_columns.is_empty());

        replace_rows(
            &pool,
            &v1,
            "pc-01",
            "job-items",
            None,
            &serde_json::json!({"items": [{"item_id": "i-1", "name": "Widget"}]}),
        )
        .await
        .unwrap();

        // Cache miss on the changed spec's DDL fingerprint: must
        // actually reconcile (rebuild, here), not report a false noop.
        let v2 = items_spec_v2();
        let change = ensure_table_cached(&pool, &v2).await.unwrap();
        assert!(
            change.rebuilt,
            "spec-fingerprinted cache must detect the primary_key change and rebuild"
        );
        assert_eq!(change.rows_copied, 1);
    }

    /// Review R1-1-1: a rebuild's temp table must never collide with —
    /// and therefore never `DROP` — another job's real explode table.
    /// Regression for the original implementation, which named the
    /// temp table `"{spec.table}__migrate"` and unconditionally
    /// `DROP TABLE IF EXISTS`-ed it: an operator (or attacker who knew
    /// the convention) naming their own table exactly that would have
    /// had it silently destroyed by an unrelated job's primary_key
    /// change. Here a second job's table is deliberately named to
    /// match what the OLD naming scheme would have picked for
    /// `example_items`'s migration, and must survive untouched.
    #[tokio::test]
    async fn rebuild_does_not_touch_a_table_named_like_the_old_temp_convention() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // A second, unrelated job's table happens to be named exactly
        // what the pre-fix temp-table convention would have used for
        // example_items's migration.
        let decoy = ExplodeSpec {
            field: "decoy".into(),
            table: "example_items__migrate".into(),
            primary_key: vec!["k".into()],
            track_history: false,
            columns: vec![ExplodeColumn {
                field: "k".into(),
                kind: Some("text".into()),
                index: false,
            }],
        };
        ensure_table(&pool, &decoy).await.unwrap();
        replace_rows(
            &pool,
            &decoy,
            "pc-99",
            "job-decoy",
            None,
            &serde_json::json!({"decoy": [{"k": "precious"}]}),
        )
        .await
        .unwrap();

        // Now trigger a rebuild of example_items (primary_key change).
        let v1 = items_spec_v1();
        ensure_table(&pool, &v1).await.unwrap();
        let v2 = items_spec_v2();
        ensure_table(&pool, &v2).await.unwrap();

        // The decoy table and its row must be completely unaffected.
        let decoy_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM example_items__migrate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(decoy_rows.0, 1, "unrelated table's row must survive");
        let k: (String,) = sqlx::query_as("SELECT k FROM example_items__migrate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(k.0, "precious");
    }

    /// Review R1-2-1: `job create`'s HTTP handler now migrates every
    /// explode spec in a manifest inside one transaction via
    /// `ensure_tables_atomic`, so a manifest with two specs where the
    /// first spec's migration would succeed but the second spec is
    /// invalid must leave NEITHER table mutated — not a half-applied
    /// state where the first table was already rebuilt while the
    /// request (and hence the stored manifest) reports failure.
    #[tokio::test]
    async fn ensure_tables_atomic_rolls_back_every_spec_when_one_fails() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // Spec A already exists under its original schema and would
        // successfully rebuild (primary_key change) if reconciled
        // alone.
        let a_v1 = items_spec_v1();
        ensure_table(&pool, &a_v1).await.unwrap();
        replace_rows(
            &pool,
            &a_v1,
            "pc-01",
            "job-items",
            None,
            &serde_json::json!({"items": [{"item_id": "i-1", "name": "Widget"}]}),
        )
        .await
        .unwrap();
        let a_v2 = items_spec_v2();

        // Spec B is invalid: its primary_key references a column that
        // isn't declared, so `create_table_sql` (called from inside
        // `reconcile_table`) rejects it.
        let mut b_bad = items_spec_v1();
        b_bad.table = "example_other".into();
        b_bad.primary_key = vec!["does_not_exist".into()];

        let err = ensure_tables_atomic(&pool, &[a_v2, b_bad])
            .await
            .expect_err("second spec is invalid; the whole batch must fail");
        assert!(err.to_string().contains("does_not_exist"));

        // Spec A's table must be untouched — still the ORIGINAL
        // schema, not rebuilt — because the failure rolled the shared
        // transaction back instead of leaving A's migration committed.
        let ddl: (String,) = sqlx::query_as(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'example_items'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            ddl.0.contains("PRIMARY KEY (pc_id, job_id, \"item_id\")"),
            "spec A's rebuild must have rolled back when spec B failed: {}",
            ddl.0
        );
        assert!(
            !ddl.0.contains("\"kind\""),
            "spec A must not carry v2's new column after rollback: {}",
            ddl.0
        );

        // And the pre-existing row is still there under the old shape.
        let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM example_items")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 1);

        // Spec B's table must not have been created at all.
        let b_exists: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'example_other'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert!(b_exists.is_none(), "spec B's table must not exist");
    }

    /// The job-create handler validates specs before writing the
    /// catalog, so a bad spec must be rejected without any DB.
    #[test]
    fn validate_specs_rejects_a_bad_spec_and_accepts_good_ones() {
        let mut bad = items_spec_v2();
        bad.primary_key = vec!["does_not_exist".into()];
        let err = validate_specs(&[items_spec_v1(), bad]).unwrap_err();
        assert!(err.to_string().contains("does_not_exist"), "{err}");
        validate_specs(&[items_spec_v1(), items_spec_v2()]).unwrap();
    }

    /// Duplicate columns and built-in-column clashes must be rejected
    /// by `validate_specs` so they never reach the catalog.
    #[test]
    fn validate_specs_rejects_duplicate_and_builtin_columns() {
        let mut dup = items_spec_v2();
        dup.columns.push(ExplodeColumn {
            field: "Name".into(),
            kind: None,
            index: false,
        });
        assert!(validate_specs(&[dup]).is_err());
        let mut builtin = items_spec_v2();
        builtin.columns.push(ExplodeColumn {
            field: "pc_id".into(),
            kind: None,
            index: false,
        });
        assert!(validate_specs(&[builtin]).is_err());
    }

    /// `job create` writes the catalog BEFORE the SQL migration, so a
    /// failed/skipped migration leaves "catalog NEW, table OLD". A
    /// backend restart (empty cache, new manifest from the catalog)
    /// must migrate the table forward and keep every row, never leave
    /// it on the old schema or drop data.
    #[tokio::test]
    async fn startup_reconcile_migrates_forward_when_catalog_is_ahead_of_table() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let rename = |mut spec: ExplodeSpec| {
            spec.table = "example_items_ahead".into();
            spec
        };
        let v1 = rename(items_spec_v1());

        ensure_table(&pool, &v1).await.unwrap();
        for (pc, id, name) in [("pc-01", "i-1", "Widget"), ("pc-02", "i-2", "Gadget")] {
            replace_rows(
                &pool,
                &v1,
                pc,
                "job-items",
                None,
                &serde_json::json!({"items": [{"item_id": id, "name": name}]}),
            )
            .await
            .unwrap();
        }
        // Restart: fresh cache, catalog already holds the v2 manifest.
        ensured_tables().lock().unwrap().remove(&v1.table);
        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "id": "job-items",
            "version": "0.0.2",
            "execute": { "shell": "powershell", "script": "echo '{}'", "timeout": "30s" },
            "inventory": {
                "display": [{ "field": "items", "label": "Items" }],
                "explode": [{
                    "field": "items",
                    "table": "example_items_ahead",
                    "primary_key": ["name"],
                    "columns": [{ "field": "item_id" }, { "field": "name" }, { "field": "kind" }],
                }],
            },
        }))
        .unwrap();
        ensure_tables_for_jobs(&pool, [manifest]).await.unwrap();

        let ddl: (String,) = sqlx::query_as(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'example_items_ahead'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(ddl.0.contains("\"kind\""), "new column present: {}", ddl.0);
        let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM example_items_ahead")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 2, "every row survives the forward migration");
    }
}
