//! Per-column sort and filter state for `GET /api/agents`.
//!
//! The SPA sends the same `table` JSON the Inventory tables use:
//! `{sort: {id, dir} | null, filters: {id: {contains?, values?}}}`.
//! Column ids come from [`BUILTINS`] (the Agents table's own column ids) or
//! `meta:<key>` for a metadata key that exists in `agent_meta`. Nothing from
//! the request is ever spliced into SQL: identifiers are `'static` literals
//! from the registry, every value (including metadata keys and checklist
//! selections) is bound.
//!
//! Ordering: NULLs sort last in both directions; the tiebreaker is always
//! `pc_id ASC` (added by the caller) so pages never overlap or skip.
use std::collections::{BTreeSet, HashMap};

use axum::http::StatusCode;
use serde::Deserialize;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};

use super::super::sql_like::contains_like;

pub(super) type ApiError = (StatusCode, String);

fn bad(message: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}

/// Most checklist values one filter may carry (matches the Inventory cap).
const MAX_VALUES: usize = 200;
/// Distinct values returned before the checklist gives way to a text box.
pub(super) const MAX_CANDIDATES: usize = 200;
const MAX_META_COLUMNS: usize = 32;
const MAX_STATE_BYTES: usize = 64 * 1024;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    sort: Option<RawSort>,
    #[serde(default)]
    filters: HashMap<String, Filter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSort {
    id: String,
    dir: String,
}

#[derive(Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Filter {
    contains: Option<String>,
    // JSON null stands for "no value", distinct from "" and from "null".
    values: Option<Vec<Option<String>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Number,
    Time,
    Status,
}

pub(super) struct Builtin {
    id: &'static str,
    /// SQL expression over the `agents` row.
    raw: &'static str,
    kind: Kind,
    /// A second expression `contains` also searches (what the cell shows as
    /// its secondary line).
    also: Option<&'static str>,
    /// `?sort=` tokens (beyond the original allow-list) that map here.
    legacy: &'static [&'static str],
}

/// The sortable / filterable Agents columns. `signing` and `actions` are
/// deliberately absent: neither has a meaningful SQL ordering.
const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "status",
        raw: "",
        kind: Kind::Status,
        also: None,
        legacy: &["status"],
    },
    Builtin {
        id: "pcId",
        raw: "pc_id",
        kind: Kind::Text,
        also: Some("hostname"),
        legacy: &[],
    },
    Builtin {
        id: "os",
        raw: "os_family",
        kind: Kind::Text,
        also: None,
        legacy: &[],
    },
    Builtin {
        id: "agent",
        raw: "agent_version",
        kind: Kind::Text,
        also: None,
        legacy: &[],
    },
    Builtin {
        id: "lastHeartbeat",
        raw: "last_heartbeat",
        kind: Kind::Time,
        also: None,
        legacy: &[],
    },
    Builtin {
        id: "lastLogon",
        raw: "COALESCE(NULLIF(last_logon_display_name,''), NULLIF(last_logon_user,''))",
        kind: Kind::Text,
        also: Some("last_logon_user"),
        legacy: &[],
    },
    Builtin {
        id: "credential",
        raw: "NULLIF(nats_user,'')",
        kind: Kind::Text,
        also: None,
        legacy: &["nats_user", "credential"],
    },
    Builtin {
        id: "cpu",
        raw: "agent_cpu_pct",
        kind: Kind::Number,
        also: None,
        legacy: &["agent_cpu_pct", "cpu"],
    },
    Builtin {
        id: "rss",
        raw: "agent_rss_bytes",
        kind: Kind::Number,
        also: None,
        legacy: &["agent_rss_bytes", "rss"],
    },
];

/// A validated column: a registry entry, or a metadata key (bound, never
/// interpolated).
#[derive(Clone)]
pub(super) enum Col {
    Builtin(&'static Builtin),
    Meta(String),
}

impl Col {
    pub(super) fn id(&self) -> String {
        match self {
            Col::Builtin(b) => b.id.to_string(),
            Col::Meta(key) => format!("meta:{key}"),
        }
    }
    fn kind(&self) -> Kind {
        match self {
            Col::Builtin(b) => b.kind,
            Col::Meta(_) => Kind::Text,
        }
    }
}

/// The column a `?sort=` token that predates the original allow-list names
/// (status / credential / cpu / rss), if any.
pub(super) fn legacy_column(token: &str) -> Option<Col> {
    BUILTINS
        .iter()
        .find(|b| b.legacy.contains(&token))
        .map(Col::Builtin)
}

/// Which expression of a column to emit.
#[derive(Clone, Copy, PartialEq)]
enum Val {
    /// The stored value (for ordering / NULL tests).
    Raw,
    /// The value as text (for `contains` / checklists / distinct).
    Text,
}

/// Emit a column expression. `cutoff` is bound only for the status column.
fn push_expr(
    qb: &mut QueryBuilder<Sqlite>,
    col: &Col,
    val: Val,
    cutoff: chrono::DateTime<chrono::Utc>,
) {
    let text = val == Val::Text;
    match col {
        Col::Builtin(b) if b.kind == Kind::Status => {
            qb.push("CASE WHEN last_heartbeat IS NOT NULL AND last_heartbeat >= ")
                .push_bind(cutoff)
                .push(" THEN 'online' ELSE 'offline' END");
        }
        Col::Builtin(b) => {
            if text {
                qb.push("CAST(");
            }
            qb.push(b.raw);
            if text {
                qb.push(" AS TEXT)");
            }
        }
        Col::Meta(key) => {
            // `value` is already TEXT; no cast needed.
            qb.push("(SELECT value FROM agent_meta WHERE pc_id = agents.pc_id AND key = ")
                .push_bind(key.clone())
                .push(")");
        }
    }
}

/// A resolved `table` state.
#[derive(Default)]
pub(super) struct Table {
    pub(super) sort: Option<(Col, bool)>,
    pub(super) filters: Vec<(Col, Filter)>,
}

async fn meta_exists(conn: &mut SqliteConnection, key: &str) -> Result<bool, ApiError> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_meta WHERE key = ?)")
        .bind(key)
        .fetch_one(conn)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "agent_meta key check");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "metadata lookup failed".to_string(),
            )
        })
}

/// Resolve one column id. Unknown ids and undeclared metadata keys are 400s.
pub(super) async fn column(conn: &mut SqliteConnection, id: &str) -> Result<Col, ApiError> {
    if let Some(key) = id.strip_prefix("meta:") {
        if key.is_empty() || !meta_exists(conn, key).await? {
            return Err(bad("unknown metadata column"));
        }
        return Ok(Col::Meta(key.to_string()));
    }
    BUILTINS
        .iter()
        .find(|b| b.id == id)
        .map(Col::Builtin)
        .ok_or_else(|| bad(format!("unknown table column: {id:?}")))
}

/// Parse and validate the `table` query param.
pub(super) async fn resolve(
    conn: &mut SqliteConnection,
    raw: Option<&str>,
) -> Result<Table, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Table::default());
    };
    if raw.len() > MAX_STATE_BYTES {
        return Err(bad("table state is too large"));
    }
    let state: RawState = serde_json::from_str(raw).map_err(|_| bad("invalid table state"))?;
    let metas: BTreeSet<&str> = state
        .filters
        .keys()
        .map(String::as_str)
        .chain(state.sort.iter().map(|s| s.id.as_str()))
        .filter(|id| id.starts_with("meta:"))
        .collect();
    if metas.len() > MAX_META_COLUMNS {
        return Err(bad("too many metadata columns"));
    }
    if state.filters.len() > BUILTINS.len() + MAX_META_COLUMNS {
        return Err(bad("too many column filters"));
    }
    let sort = match state.sort {
        None => None,
        Some(s) => {
            let desc = match s.dir.as_str() {
                "asc" => false,
                "desc" => true,
                _ => return Err(bad("invalid sort direction")),
            };
            Some((column(conn, &s.id).await?, desc))
        }
    };
    // Sorted so the generated SQL (and thus statement cache) is stable.
    let mut entries: Vec<_> = state.filters.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut filters = Vec::with_capacity(entries.len());
    for (id, filter) in entries {
        if filter.values.as_ref().is_some_and(|v| v.len() > MAX_VALUES) {
            return Err(bad("too many checklist values"));
        }
        filters.push((column(conn, &id).await?, filter));
    }
    Ok(Table { sort, filters })
}

/// Append `AND <condition>` for every column filter except `skip` (the
/// column whose candidates are being listed). The caller has already emitted
/// a `WHERE`, so this only ever appends `AND`s through `sep`.
pub(super) fn push_conditions(
    qb: &mut QueryBuilder<Sqlite>,
    started: &mut bool,
    table: &Table,
    skip: Option<&str>,
    cutoff: chrono::DateTime<chrono::Utc>,
) {
    for (col, filter) in &table.filters {
        if skip == Some(col.id().as_str()) {
            continue;
        }
        if let Some(contains) = filter
            .contains
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            qb.push(super::sep(started)).push("(");
            push_expr(qb, col, Val::Text, cutoff);
            qb.push(" LIKE ")
                .push_bind(contains_like(contains))
                .push(" ESCAPE '\\'");
            if let Col::Builtin(Builtin {
                also: Some(also), ..
            }) = col
            {
                qb.push(format!(" OR CAST({also} AS TEXT) LIKE "))
                    .push_bind(contains_like(contains))
                    .push(" ESCAPE '\\'");
            }
            qb.push(")");
        }
        if let Some(values) = &filter.values {
            qb.push(super::sep(started)).push("(0");
            for value in values {
                match value {
                    Some(v) => {
                        qb.push(" OR ");
                        push_expr(qb, col, Val::Text, cutoff);
                        qb.push(" = ").push_bind(v.clone());
                    }
                    None => {
                        qb.push(" OR ");
                        push_expr(qb, col, Val::Raw, cutoff);
                        qb.push(" IS NULL");
                    }
                }
            }
            qb.push(")");
        }
    }
}

/// Append ` ORDER BY …, pc_id ASC` for a column sort.
pub(super) fn push_order(
    qb: &mut QueryBuilder<Sqlite>,
    col: &Col,
    desc: bool,
    cutoff: chrono::DateTime<chrono::Utc>,
) {
    let dir = if desc { " DESC" } else { " ASC" };
    qb.push(" ORDER BY ");
    let order_expr = |qb: &mut QueryBuilder<Sqlite>| match col.kind() {
        Kind::Number => {
            qb.push("CAST(");
            push_expr(qb, col, Val::Raw, cutoff);
            qb.push(" AS REAL)");
        }
        Kind::Time => {
            qb.push("julianday(");
            push_expr(qb, col, Val::Raw, cutoff);
            qb.push(")");
        }
        Kind::Status | Kind::Text => push_expr(qb, col, Val::Raw, cutoff),
    };
    if col.kind() != Kind::Status {
        order_expr(qb);
        qb.push(" IS NULL, ");
    }
    order_expr(qb);
    if col.kind() == Kind::Text {
        qb.push(" COLLATE NOCASE");
    }
    qb.push(dir).push(", pc_id ASC");
}

/// The text a checklist offers for `col` (status: online / offline).
pub(super) fn push_candidate(
    qb: &mut QueryBuilder<Sqlite>,
    col: &Col,
    cutoff: chrono::DateTime<chrono::Utc>,
) {
    push_expr(qb, col, Val::Text, cutoff);
}
