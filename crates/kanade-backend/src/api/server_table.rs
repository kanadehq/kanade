//! Shared controlled table state, typed columns and bound filter conditions.
use super::sql_like::contains_like;
use axum::http::StatusCode;
use serde::Deserialize;
use sqlx::{QueryBuilder, Sqlite};
use std::collections::HashMap;

/// Maximum checklist selection, shared by request validation and SQL generation.
pub(crate) const MAX_VALUES: usize = 200;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableState {
    pub(crate) sort: Option<Sort>,
    #[serde(default)]
    pub(crate) filters: HashMap<String, Filter>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sort {
    pub(crate) id: String,
    pub(crate) dir: String,
}
#[derive(Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Filter {
    pub(crate) contains: Option<String>,
    // JSON null represents missing values without colliding with any string.
    pub(crate) values: Option<Vec<Option<String>>>,
}

#[derive(Clone, Copy)]
pub(crate) enum ValueKind {
    Text,
    Number,
    Timestamp,
    Array,
}
pub(crate) struct TypedColumn {
    pub(crate) id: String,
    pub(crate) raw: String,
    pub(crate) order: String,
    pub(crate) kind: ValueKind,
}
impl TypedColumn {
    pub(crate) fn new(id: impl Into<String>, raw: impl Into<String>, kind: Option<&str>) -> Self {
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
    pub(crate) fn text(&self) -> String {
        match self.kind {
            ValueKind::Timestamp => format!(
                "CASE WHEN {} IS NOT NULL THEN CAST({} AS TEXT) END",
                self.order, self.raw
            ),
            _ => format!("CAST({} AS TEXT)", self.order),
        }
    }
}

/// Validated column expressions; implementations bind any dynamic keys.
pub(crate) trait Column {
    fn expression(&self, q: &mut QueryBuilder<Sqlite>, text: bool);
    fn secondary(&self) -> Option<&str> {
        None
    }
}

impl Column for TypedColumn {
    fn expression(&self, q: &mut QueryBuilder<Sqlite>, text: bool) {
        q.push(if text {
            self.text()
        } else {
            self.order.clone()
        });
    }
}

/// Append a filter using validated expressions supplied by the column registry.
/// `text` selects the display text; otherwise the expression must preserve NULL.
pub(crate) fn conditions(
    q: &mut QueryBuilder<Sqlite>,
    started: &mut bool,
    filter: &Filter,
    column: &impl Column,
) -> Result<(), (StatusCode, String)> {
    if filter.values.as_ref().is_some_and(|v| v.len() > MAX_VALUES) {
        return Err((StatusCode::BAD_REQUEST, "too many checklist values".into()));
    }
    let mut clause = |q: &mut QueryBuilder<Sqlite>| {
        q.push(if *started { " AND " } else { " WHERE " });
        *started = true;
    };
    if let Some(contains) = filter
        .contains
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        clause(q);
        q.push("(");
        column.expression(q, true);
        q.push(" LIKE ")
            .push_bind(contains_like(contains))
            .push(" ESCAPE '\\'");
        if let Some(also) = column.secondary() {
            q.push(format!(" OR CAST({also} AS TEXT) LIKE "))
                .push_bind(contains_like(contains))
                .push(" ESCAPE '\\'");
        }
        q.push(")");
    }
    if let Some(values) = &filter.values {
        clause(q);
        q.push("(0");
        for value in values {
            q.push(" OR ");
            column.expression(q, value.is_some());
            if let Some(value) = value {
                q.push(" = ").push_bind(value.clone());
            } else {
                q.push(" IS NULL");
            }
        }
        q.push(")");
    }
    Ok(())
}
