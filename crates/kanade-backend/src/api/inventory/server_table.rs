//! SQL-backed table state. Identifiers come only from the column registry;
//! request values (including checklist selections) are always bound.
//!
//! `table` is JSON `{sort: {id, dir} | null, filters: {id: {contains?, values?}}}`.
//! IDs are `pcId`, `lastLogon`, `collectedAt`/`collected`, `f:<manifest field>`
//! or `meta:<existing key>`. Checklist values are strings or JSON null.
//! `format=page` opts search into `{rows,total}`; legacy array clients remain
//! unchanged. `distinct=<id>` returns `{values:[{value,count}],overflow}`
//! over the complete match set, excluding that column's table filter.
use super::*;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

type ApiError = (StatusCode, String);
fn bad(message: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn db(error: sqlx::Error) -> ApiError {
    warn!(%error, "inventory table query");
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TableState {
    sort: Option<Sort>,
    #[serde(default)]
    filters: HashMap<String, Filter>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sort {
    id: String,
    dir: String,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    contains: Option<String>,
    // JSON null represents missing values without colliding with any string.
    values: Option<Vec<Option<String>>>,
}

#[derive(Clone, Copy)]
enum ValueKind {
    Text,
    Number,
    Timestamp,
    Array,
}
struct Column {
    id: String,
    raw: String,
    order: String,
    kind: ValueKind,
}
impl Column {
    fn new(id: impl Into<String>, raw: impl Into<String>, kind: Option<&str>) -> Self {
        let raw = raw.into();
        let kind = match kind {
            Some("number" | "bytes" | "integer" | "real") => ValueKind::Number,
            Some("timestamp") => ValueKind::Timestamp,
            Some("table") => ValueKind::Array,
            _ => ValueKind::Text,
        };
        let order = match kind {
            ValueKind::Number => format!(
                "CASE WHEN typeof({raw}) IN ('integer','real') THEN CAST({raw} AS REAL) \
                 WHEN json_valid({raw}) THEN CASE WHEN json_type({raw}) IN ('integer','real') \
                 THEN CAST({raw} AS REAL) END END"
            ),
            ValueKind::Timestamp => format!("julianday({raw})"),
            ValueKind::Array => format!(
                "CASE WHEN json_valid({raw}) THEN CASE WHEN json_type({raw}) = 'array' THEN json_array_length({raw}) END END"
            ),
            ValueKind::Text => format!("CAST({raw} AS TEXT) COLLATE NOCASE"),
        };
        Self {
            id: id.into(),
            raw,
            order,
            kind,
        }
    }
    fn text(&self) -> String {
        match self.kind {
            ValueKind::Timestamp => format!(
                "CASE WHEN {} IS NOT NULL THEN CAST({} AS TEXT) END",
                self.order, self.raw
            ),
            _ => format!("CAST({} AS TEXT)", self.order),
        }
    }
}

pub(super) enum Source<'a> {
    Fleet(&'a InventoryHint),
    Scalars(&'a InventoryHint),
    Explode(&'a ExplodeSpec),
}
struct Registry {
    from: String,
    columns: Vec<Column>,
    select: String,
    tie: String,
}
impl Registry {
    fn column(&self, id: &str) -> Result<&Column, ApiError> {
        self.columns
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| bad(format!("unknown table column: {id:?}")))
    }
    fn new(source: &Source<'_>) -> Result<Self, ApiError> {
        let account = "CASE WHEN NULLIF(trim(a.last_logon_display_name),'') IS NOT NULL \
            AND NULLIF(trim(a.last_logon_user),'') IS NOT NULL \
            AND a.last_logon_display_name <> a.last_logon_user \
            THEN a.last_logon_display_name || ' (' || a.last_logon_user || ')' \
            WHEN NULLIF(trim(a.last_logon_display_name),'') IS NOT NULL THEN a.last_logon_display_name \
            WHEN NULLIF(trim(a.last_logon_user),'') IS NOT NULL THEN a.last_logon_user END";
        let mut columns = vec![
            Column::new("pcId", "f.pc_id", None),
            Column::new("lastLogon", account, None),
            Column::new("collectedAt", "f.collected_at", Some("timestamp")),
            Column::new("collected", "f.collected_at", Some("timestamp")),
        ];
        let mut select =
            "f.pc_id, f.collected_at, a.last_logon_user, a.last_logon_display_name".to_string();
        let mut tie = "f.pc_id, f.job_id".to_string();
        let table = match source {
            Source::Explode(spec) => {
                validate_ident(&spec.table).map_err(|e| bad(e.to_string()))?;
                for c in &spec.columns {
                    validate_ident(&c.field).map_err(|e| bad(e.to_string()))?;
                    let raw = format!("f.\"{}\"", c.field);
                    columns.push(Column::new(
                        format!("f:{}", c.field),
                        &raw,
                        c.kind.as_deref(),
                    ));
                    select.push_str(&format!(", {raw}"));
                }
                for key in &spec.primary_key {
                    if !spec.columns.iter().any(|c| c.field == *key) {
                        return Err(bad("invalid primary key"));
                    }
                    tie.push_str(&format!(", f.\"{key}\""));
                }
                format!("\"{}\"", spec.table)
            }
            Source::Fleet(hint) | Source::Scalars(hint) => {
                let fields = if matches!(source, Source::Fleet(_)) {
                    hint.summary.as_ref().unwrap_or(&hint.display).clone()
                } else {
                    hint.display
                        .iter()
                        .filter(|c| c.kind.as_deref() != Some("table"))
                        .cloned()
                        .collect()
                };
                for c in &fields {
                    validate_ident(&c.field).map_err(|e| bad(e.to_string()))?;
                    columns.push(Column::new(
                        format!("f:{}", c.field),
                        format!("json_extract(f.facts_json, '$.{}')", c.field),
                        c.kind.as_deref(),
                    ));
                }
                if matches!(source, Source::Fleet(_)) {
                    select.push_str(", f.facts_json");
                } else {
                    let projection = fields
                        .iter()
                        .map(|c| format!("'{0}', json_extract(f.facts_json, '$.{0}')", c.field))
                        .collect::<Vec<_>>()
                        .join(", ");
                    select.push_str(&format!(", json_object({projection}) AS projected_json"));
                }
                "inventory_facts".to_string()
            }
        };
        Ok(Self {
            from: format!("{table} f LEFT JOIN agents a ON a.pc_id = f.pc_id"),
            columns,
            select,
            tie,
        })
    }
}

fn conditions(
    q: &mut QueryBuilder<Sqlite>,
    reg: &Registry,
    state: &TableState,
    skip: Option<&str>,
) -> Result<(), ApiError> {
    for (id, filter) in &state.filters {
        let column = reg.column(id)?;
        if Some(id.as_str()) == skip {
            continue;
        }
        let text = column.text();
        if let Some(contains) = filter.contains.as_deref().filter(|s| !s.trim().is_empty()) {
            let escaped = contains
                .trim()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            q.push(format!(" AND {text} LIKE "))
                .push_bind(format!("%{escaped}%"))
                .push(" ESCAPE '\\'");
        }
        if let Some(values) = &filter.values {
            if values.len() > 200 {
                return Err(bad("too many checklist values"));
            }
            q.push(" AND (");
            q.push("0");
            for value in values {
                if let Some(value) = value {
                    q.push(format!(" OR {text} = ")).push_bind(value.clone());
                } else {
                    q.push(format!(" OR {} IS NULL", column.order));
                }
            }
            q.push(")");
        }
    }
    Ok(())
}

// Legacy search form conditions share the same WHERE clause with table
// filters, count and candidates. The UI's column ids are distinct from
// manifest fields; neither is used as an identifier without resolution.
fn legacy_conditions(
    q: &mut QueryBuilder<Sqlite>,
    reg: &Registry,
    params: &HashMap<String, String>,
) -> Result<(), ApiError> {
    for (key, value) in params {
        if ["table", "format", "distinct", "limit", "offset", "q"].contains(&key.as_str()) {
            continue;
        }
        let (name, op) = key.split_once("__").unwrap_or((key, "eq"));
        let id = if name == "pc_id" {
            "pcId".to_string()
        } else {
            format!("f:{name}")
        };
        let col = reg.column(&id)?;
        let cmp = match op {
            "eq" => "=",
            "ne" => "<>",
            "lt" => "<",
            "le" => "<=",
            "gt" => ">",
            "ge" => ">=",
            "contains" | "prefix" | "suffix" => "LIKE",
            _ => return Err(bad("unknown filter operator")),
        };
        if cmp == "LIKE" {
            let escaped = value
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = match op {
                "prefix" => format!("{escaped}%"),
                "suffix" => format!("%{escaped}"),
                _ => format!("%{escaped}%"),
            };
            q.push(format!(" AND {} LIKE ", col.raw))
                .push_bind(pattern)
                .push(" ESCAPE '\\'");
        } else {
            // SQLite column affinities do not apply to JSON expressions.
            q.push(format!(" AND {} {cmp} ", col.order));
            if matches!(col.kind, ValueKind::Number) {
                let n: f64 = value
                    .parse()
                    .map_err(|_| bad("numeric filter requires a number"))?;
                if !n.is_finite() {
                    return Err(bad("numeric filter requires a finite number"));
                }
                q.push_bind(n);
            } else if matches!(col.kind, ValueKind::Timestamp) {
                q.push("julianday(").push_bind(value.clone()).push(")");
            } else {
                q.push_bind(value.clone());
            }
        }
    }
    Ok(())
}

pub(super) fn enabled(params: &HashMap<String, String>) -> bool {
    params.contains_key("table")
        || params.get("format").is_some_and(|f| f == "page")
        || params.contains_key("distinct")
}

pub(super) async fn run(
    pool: &SqlitePool,
    manifest: &str,
    source: Source<'_>,
    params: &HashMap<String, String>,
) -> Result<Value, ApiError> {
    if params.get("table").is_some_and(|s| s.len() > 64 * 1024) {
        return Err(bad("table state is too large"));
    }
    let mut registry = Registry::new(&source)?;
    let state: TableState =
        serde_json::from_str(params.get("table").map(String::as_str).unwrap_or("{}"))
            .map_err(|_| bad("invalid table state"))?;
    // Metadata keys are data, never SQL identifiers. Only existing keys
    // may be requested, and each join binds its key independently.
    let mut meta_keys = std::collections::BTreeSet::new();
    for id in state
        .filters
        .keys()
        .map(String::as_str)
        .chain(state.sort.iter().map(|s| s.id.as_str()))
        .chain(params.get("distinct").map(String::as_str))
    {
        if let Some(key) = id.strip_prefix("meta:") {
            meta_keys.insert(key.to_string());
        }
    }
    if meta_keys.len() > 32 {
        return Err(bad("too many metadata columns"));
    }
    for (index, key) in meta_keys.iter().enumerate() {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_meta WHERE key = ?)")
                .bind(key)
                .fetch_one(pool)
                .await
                .map_err(db)?;
        if !exists {
            return Err(bad("unknown metadata column"));
        }
        registry.columns.push(Column::new(
            format!("meta:{key}"),
            format!("m{index}.value"),
            None,
        ));
    }
    if state.filters.len() > registry.columns.len() {
        return Err(bad("too many column filters"));
    }
    let order = if let Some(sort) = &state.sort {
        let column = registry.column(&sort.id)?;
        let dir = match sort.dir.as_str() {
            "asc" => "ASC",
            "desc" => "DESC",
            _ => return Err(bad("invalid sort direction")),
        };
        format!(
            "{} IS NULL, {} {dir}, {}",
            column.order, column.order, registry.tie
        )
    } else {
        registry.tie.clone()
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(
            1,
            if matches!(source, Source::Fleet(_)) {
                1000
            } else {
                5000
            },
        );
    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let base = |projection: String, skip: Option<&str>| -> Result<QueryBuilder<Sqlite>, ApiError> {
        let mut query = QueryBuilder::new(format!("SELECT {projection} FROM {} ", registry.from));
        for (index, key) in meta_keys.iter().enumerate() {
            query
                .push(format!(
                    " LEFT JOIN agent_meta m{index} ON m{index}.pc_id = f.pc_id AND m{index}.key = "
                ))
                .push_bind(key.clone());
        }
        query
            .push(" WHERE f.job_id = ")
            .push_bind(manifest.to_string());
        if let Some(q) = params.get("q").filter(|q| !q.is_empty()) {
            let escaped = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            query
                .push(" AND f.pc_id LIKE ")
                .push_bind(format!("%{escaped}%"))
                .push(" ESCAPE '\\'");
        }
        legacy_conditions(&mut query, &registry, params)?;
        conditions(&mut query, &registry, &state, skip)?;
        Ok(query)
    };
    let mut tx = pool.begin().await.map_err(db)?;
    if let Some(id) = params.get("distinct") {
        let column = registry.column(id)?;
        let mut q = base(
            format!("{} AS value, COUNT(*) AS n", column.text()),
            Some(id),
        )?;
        q.push(" GROUP BY value ORDER BY value IS NULL, value LIMIT 201");
        let rows = q.build().fetch_all(&mut *tx).await.map_err(db)?;
        let overflow = rows.len() > 200;
        let values: Vec<Value> = if overflow {
            vec![]
        } else {
            rows.into_iter().map(|r| json!({"value": r.get::<Option<String>, _>("value"), "count": r.get::<i64, _>("n")})).collect()
        };
        tx.commit().await.map_err(db)?;
        return Ok(json!({"values": values, "overflow": overflow}));
    }
    let total: i64 = base("COUNT(*)".into(), None)?
        .build_query_scalar()
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let mut q = base(registry.select.clone(), None)?;
    q.push(format!(" ORDER BY {order} LIMIT "))
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    let rows = q.build().fetch_all(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::new();
        obj.insert("pc_id".into(), json!(row.get::<String, _>("pc_id")));
        obj.insert(
            "collected_at".into(),
            json!(
                row.try_get::<Option<DateTime<Utc>>, _>("collected_at")
                    .ok()
                    .flatten()
            ),
        );
        let user = row.get::<Option<String>, _>("last_logon_user");
        let display_name = row.get::<Option<String>, _>("last_logon_display_name");
        obj.insert(ACCOUNT_USER_KEY.into(), json!(user));
        obj.insert(ACCOUNT_DISPLAY_NAME_KEY.into(), json!(display_name));
        match &source {
            Source::Fleet(_) => {
                obj.insert("last_logon_user".into(), json!(user));
                obj.insert("last_logon_display_name".into(), json!(display_name));
                obj.insert(
                    "facts".into(),
                    serde_json::from_str(&row.get::<String, _>("facts_json"))
                        .unwrap_or(Value::Null),
                );
            }
            Source::Scalars(hint) => {
                let facts: Value = serde_json::from_str(&row.get::<String, _>("projected_json"))
                    .unwrap_or(Value::Null);
                for col in scalar_columns(hint) {
                    obj.insert(
                        col.field.clone(),
                        facts.get(&col.field).cloned().unwrap_or(Value::Null),
                    );
                }
            }
            Source::Explode(spec) => {
                for col in &spec.columns {
                    let value = match col.kind.as_deref() {
                        Some("integer") => json!(
                            row.try_get::<Option<i64>, _>(col.field.as_str())
                                .ok()
                                .flatten()
                        ),
                        Some("real") => json!(
                            row.try_get::<Option<f64>, _>(col.field.as_str())
                                .ok()
                                .flatten()
                        ),
                        _ => json!(
                            row.try_get::<Option<String>, _>(col.field.as_str())
                                .ok()
                                .flatten()
                        ),
                    };
                    obj.insert(col.field.clone(), value);
                }
            }
        }
        out.push(obj);
    }
    match source {
        Source::Fleet(hint) => Ok(
            json!({"manifest_id": manifest, "display": hint.display, "summary": hint.summary, "rows": out, "total": total, "limit": limit, "offset": offset}),
        ),
        _ if params.get("format").is_some_and(|f| f == "page") => {
            Ok(json!({"rows": out, "total": total}))
        }
        _ => Ok(json!(out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_job_query_extracts_paging_and_opt_in_state_together() {
        let uri = "/?limit=2&offset=1&q=pc&format=page&table=%7B%7D"
            .parse()
            .unwrap();
        let Query(params) = Query::<ByJobParams>::try_from_uri(&uri).unwrap();
        assert_eq!(params.limit, Some(2));
        assert_eq!(params.offset, Some(1));
        assert_eq!(params.q.as_deref(), Some("pc"));
        assert!(enabled(&params.table_params));
        assert_eq!(params.table_params["table"], "{}");
    }

    async fn fixture() -> (SqlitePool, InventoryHint) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE agents (pc_id TEXT PRIMARY KEY, last_logon_user TEXT, last_logon_display_name TEXT)").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE inventory_facts (pc_id TEXT, job_id TEXT, facts_json TEXT, collected_at TEXT, PRIMARY KEY(pc_id, job_id))").execute(&pool).await.unwrap();
        for (pc, n, label, date) in [
            ("a", json!(10), "zebra", "2026-01-01T01:00:00+02:00"),
            ("b", json!(2), "apple", "2026-01-01T00:00:00Z"),
            ("c", Value::Null, "banana", "invalid"),
            ("d", json!("invalid"), "apple", "2026-02-01T00:00:00Z"),
            ("e", json!(2), "apple", "2026-02-01T00:00:00Z"),
        ] {
            sqlx::query("INSERT INTO inventory_facts VALUES (?, 'job', ?, ?)")
                .bind(pc)
                .bind(json!({"n":n,"label":label,"date":date}).to_string())
                .bind(date)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO inventory_facts VALUES ('foreign','other','{\"n\":0}',NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE agent_meta (pc_id TEXT, key TEXT, value TEXT, PRIMARY KEY(pc_id,key))",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO agent_meta VALUES ('a','site','Tokyo'),('b','site','Osaka')")
            .execute(&pool)
            .await
            .unwrap();
        let hint = serde_json::from_value(json!({"display": [
            {"field":"n", "label":"Number", "type":"number"},
            {"field":"label", "label":"Label"},
            {"field":"date", "label":"Date", "type":"timestamp"}
        ]}))
        .unwrap();
        (pool, hint)
    }
    fn params(state: Value) -> HashMap<String, String> {
        HashMap::from([
            ("format".into(), "page".into()),
            ("table".into(), state.to_string()),
            ("limit".into(), "2".into()),
        ])
    }
    fn pcs(page: &Value) -> Vec<&str> {
        page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["pc_id"].as_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn numeric_sort_crosses_pages_with_nulls_last_and_stable_ties() {
        let (pool, hint) = fixture().await;
        for (dir, expected) in [
            ("asc", vec!["b", "e", "a", "c", "d"]),
            ("desc", vec!["a", "b", "e", "c", "d"]),
        ] {
            let mut query = params(json!({"sort":{"id":"f:n","dir":dir}}));
            let mut actual = vec![];
            for offset in [0, 2, 4] {
                query.insert("offset".into(), offset.to_string());
                let page = run(&pool, "job", Source::Scalars(&hint), &query)
                    .await
                    .unwrap();
                assert_eq!(page["total"], 5);
                actual.extend(pcs(&page).into_iter().map(str::to_string));
            }
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn text_and_timestamps_use_their_declared_order() {
        let (pool, hint) = fixture().await;
        for (column, expected) in [
            ("f:label", vec!["b", "d", "e", "c", "a"]),
            ("f:date", vec!["a", "b", "d", "e", "c"]),
        ] {
            let mut query = params(json!({"sort":{"id":column,"dir":"asc"}}));
            query.insert("limit".into(), "10".into());
            let page = run(&pool, "job", Source::Fleet(&hint), &query)
                .await
                .unwrap();
            assert_eq!(pcs(&page), expected);
        }
    }

    #[tokio::test]
    async fn filters_counts_and_global_candidates_share_conditions() {
        let (pool, hint) = fixture().await;
        let mut query =
            params(json!({"filters":{"f:label":{"contains":"APP","values":["apple"]}}}));
        query.insert("offset".into(), "2".into());
        let page = run(&pool, "job", Source::Scalars(&hint), &query)
            .await
            .unwrap();
        assert_eq!(page["total"], 3);
        assert_eq!(pcs(&page), ["e"]);
        query.insert("distinct".into(), "f:label".into());
        let candidates = run(&pool, "job", Source::Scalars(&hint), &query)
            .await
            .unwrap();
        assert_eq!(candidates["values"].as_array().unwrap().len(), 3);
        assert_eq!(candidates["values"][0]["count"], 3);
        query = params(json!({"filters":{"f:n":{"values":[null]}}}));
        let page = run(&pool, "job", Source::Scalars(&hint), &query)
            .await
            .unwrap();
        assert_eq!(pcs(&page), ["c", "d"]);
    }

    #[tokio::test]
    async fn rejects_unknown_identifiers_and_directions_and_binds_values() {
        let (pool, hint) = fixture().await;
        for state in [
            json!({"sort":{"id":"pcId; DROP TABLE agents", "dir":"asc"}}),
            json!({"sort":{"id":"pcId", "dir":"asc; DROP TABLE agents"}}),
            json!({"filters":{"f:unknown":{"contains":"a"}}}),
        ] {
            assert_eq!(
                run(&pool, "job", Source::Scalars(&hint), &params(state))
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let page = run(
            &pool,
            "job",
            Source::Scalars(&hint),
            &params(json!({"filters":{"f:label":{"contains":"' OR 1=1 --"}}})),
        )
        .await
        .unwrap();
        assert_eq!(page["total"], 0);
    }

    #[tokio::test]
    async fn excessive_distinct_values_return_no_partial_checklist() {
        let (pool, hint) = fixture().await;
        for i in 0..201 {
            sqlx::query("INSERT INTO inventory_facts VALUES (?, 'job', ?, NULL)")
                .bind(format!("pc{i}"))
                .bind(json!({"label":format!("value{i}")}).to_string())
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut query = params(json!({}));
        query.insert("distinct".into(), "f:label".into());
        let page = run(&pool, "job", Source::Scalars(&hint), &query)
            .await
            .unwrap();
        assert_eq!(page, json!({"values":[], "overflow":true}));
    }

    #[tokio::test]
    async fn exploded_rows_are_scoped_to_job_and_tied_by_full_primary_key() {
        let (pool, _) = fixture().await;
        let spec: ExplodeSpec = serde_json::from_value(json!({"field":"items","table":"items","primary_key":["name","source"],"columns":[{"field":"name"},{"field":"source"},{"field":"n","type":"integer"}]})).unwrap();
        crate::projector::explode::ensure_table(&pool, &spec)
            .await
            .unwrap();
        for (job, name, source, n) in [
            ("job", "x", "b", 2),
            ("job", "x", "a", 2),
            ("other", "y", "z", 0),
            ("job", "z", "a", 10),
        ] {
            sqlx::query("INSERT INTO items(pc_id,job_id,name,source,n) VALUES ('a',?,?,?,?)")
                .bind(job)
                .bind(name)
                .bind(source)
                .bind(n)
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut query = params(json!({"sort":{"id":"f:n","dir":"asc"}}));
        query.insert("limit".into(), "1".into());
        let first = run(&pool, "job", Source::Explode(&spec), &query)
            .await
            .unwrap();
        assert_eq!(first["total"], 3);
        assert_eq!(first["rows"][0]["source"], "a");
        query.insert("offset".into(), "1".into());
        let second = run(&pool, "job", Source::Explode(&spec), &query)
            .await
            .unwrap();
        assert_eq!(second["rows"][0]["source"], "b");
    }

    #[tokio::test]
    async fn metadata_keys_are_bound_and_values_are_global() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE agents(pc_id TEXT, last_logon_user TEXT, last_logon_display_name TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE TABLE inventory_facts(pc_id TEXT, job_id TEXT, facts_json TEXT, collected_at TEXT)").execute(&pool).await.unwrap();
        sqlx::query(
            "CREATE TABLE agent_meta(pc_id TEXT, key TEXT, value TEXT, PRIMARY KEY(pc_id,key))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let key = "site' OR 1=1 --";
        for (pc, value) in [("a", "Tokyo"), ("b", "Osaka")] {
            sqlx::query("INSERT INTO inventory_facts VALUES (?, 'job', '{}', NULL)")
                .bind(pc)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO agent_meta VALUES (?, ?, ?)")
                .bind(pc)
                .bind(key)
                .bind(value)
                .execute(&pool)
                .await
                .unwrap();
        }
        let hint: InventoryHint = serde_json::from_value(json!({"display":[]})).unwrap();
        let id = format!("meta:{key}");
        let mut query = HashMap::from([
            ("format".into(), "page".into()),
            ("limit".into(), "1".into()),
            (
                "table".into(),
                json!({"sort":{"id":id,"dir":"asc"}}).to_string(),
            ),
        ]);
        let page = run(&pool, "job", Source::Fleet(&hint), &query)
            .await
            .unwrap();
        assert_eq!(page["rows"][0]["pc_id"], "b");
        query.insert("distinct".into(), id);
        let candidates = run(&pool, "job", Source::Fleet(&hint), &query)
            .await
            .unwrap();
        assert_eq!(candidates["values"].as_array().unwrap().len(), 2);
        query.insert("distinct".into(), "meta:nonexistent".into());
        assert_eq!(
            run(&pool, "job", Source::Fleet(&hint), &query)
                .await
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    #[tokio::test]
    async fn array_summary_sorts_by_count_and_invalid_values_are_missing() {
        let (pool, _) = fixture().await;
        let hint: InventoryHint = serde_json::from_value(
            json!({"display":[{"field":"disks","label":"Disks","type":"table"}]}),
        )
        .unwrap();
        for (pc, value) in [
            ("a", json!([1, 2, 3])),
            ("b", json!([1, 2])),
            ("c", Value::Null),
            ("d", json!("invalid")),
            ("e", json!([])),
        ] {
            sqlx::query("UPDATE inventory_facts SET facts_json=json_set(facts_json,'$.disks',json(?)) WHERE pc_id=?").bind(value.to_string()).bind(pc).execute(&pool).await.unwrap();
        }
        let mut query = params(json!({"sort":{"id":"f:disks","dir":"asc"}}));
        query.insert("limit".into(), "10".into());
        let page = run(&pool, "job", Source::Fleet(&hint), &query)
            .await
            .unwrap();
        assert_eq!(pcs(&page), ["e", "b", "a", "c", "d"]);
    }

    #[tokio::test]
    async fn null_empty_and_literal_null_are_distinct_checklist_values() {
        let (pool, hint) = fixture().await;
        for (pc, value) in [("c", Value::Null), ("d", json!("")), ("e", json!("null"))] {
            sqlx::query("UPDATE inventory_facts SET facts_json=json_set(facts_json,'$.label',json(?)) WHERE pc_id=?").bind(value.to_string()).bind(pc).execute(&pool).await.unwrap();
        }
        for (value, expected) in [(Value::Null, "c"), (json!(""), "d"), (json!("null"), "e")] {
            let page = run(
                &pool,
                "job",
                Source::Scalars(&hint),
                &params(json!({"filters":{"f:label":{"values":[value]}}})),
            )
            .await
            .unwrap();
            assert_eq!(pcs(&page), [expected]);
        }
    }
}
