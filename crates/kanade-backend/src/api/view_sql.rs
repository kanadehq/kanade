//! #vuln-roadmap PR3: compute a [`SqlWidget`] into a render-ready
//! [`WidgetData`], with a small in-memory materialization cache.
//!
//! A SQL-backed view widget carries a raw read-only `SELECT` (a correlation
//! join over the projector's inventory / feed / compliance tables) plus a
//! [`RenderSpec`] naming how the result columns map to a visual. This module:
//!   1. runs the query in the shared read-only sandbox ([`super::query`]),
//!   2. maps `columns × rows` onto the chosen chart via [`map_widget_data`],
//!   3. caches the result per widget on the `refresh` cadence, so an expensive
//!      join doesn't re-run on every ~30s Analytics/Dashboard poll.
//!
//! The cache is in-memory (a derived, self-healing cache — no durability
//! needed): keyed by `(view_id, widget index)` with a content hash so an
//! edited query recomputes immediately instead of serving a stale shape.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use kanade_shared::manifest::{RenderKind, RenderSpec, SqlWidget};
use serde_json::Value as JsonValue;

use super::AppState;
use super::analytics::{BarRow, WidgetData};
use super::query::{ReadOnlyError, run_read_only};

/// Row cap for a view query. A correlation usually aggregates to a handful of
/// rows; a `table` widget over raw rows is bounded here so a runaway `SELECT *`
/// can't balloon a cached widget. Sits under the sandbox's own `MAX_LIMIT`.
const VIEW_ROW_LIMIT: usize = 1_000;

/// In-memory materialization cache — one entry per `(view_id, widget index)`.
/// `AppState` holds an `Arc` clone, so it's shared across requests.
pub type SqlViewCache = Arc<Mutex<HashMap<String, CacheEntry>>>;

/// A cached widget computation. `hash` fingerprints the widget's query+render
/// so an operator edit (same key, new content) is treated as a miss rather
/// than served stale until the cadence elapses.
pub struct CacheEntry {
    hash: u64,
    computed_at: Instant,
    result: Result<WidgetData, String>,
}

/// Fresh, empty cache for `AppState`.
pub fn new_cache() -> SqlViewCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Fingerprint the parts of a widget whose change should invalidate the cache
/// (the query and how it renders — not the title/description/placement, which
/// don't affect the computed data).
fn widget_hash(w: &SqlWidget) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    w.query.hash(&mut h);
    // `RenderSpec` derives `Hash`, so fingerprint it directly — no JSON
    // serialization on every cache check (Gemini).
    w.render.hash(&mut h);
    w.refresh.hash(&mut h);
    h.finish()
}

/// Compute a widget, serving a fresh-enough cache entry when present. On a
/// miss / stale / edited entry, re-runs the query and stores the outcome
/// (errors are cached too, so a broken query doesn't re-run every poll — a
/// fixed query changes the hash and recomputes immediately).
pub async fn compute_cached(
    state: &AppState,
    view_id: &str,
    idx: usize,
    widget: &SqlWidget,
) -> Result<WidgetData, String> {
    let key = format!("{view_id}\u{1f}{idx}");
    let hash = widget_hash(widget);
    let ttl = widget.refresh_interval();
    {
        let cache = state.sql_view_cache.lock().expect("sql_view_cache mutex");
        if let Some(e) = cache.get(&key) {
            if e.hash == hash && e.computed_at.elapsed() < ttl {
                return e.result.clone();
            }
        }
    }
    // Recompute outside the lock (the query awaits). A concurrent duplicate
    // recompute is harmless — both write the same key, last wins.
    let result = compute(&state.query_pool, widget).await;
    let mut cache = state.sql_view_cache.lock().expect("sql_view_cache mutex");
    cache.insert(
        key,
        CacheEntry {
            hash,
            computed_at: Instant::now(),
            result: result.clone(),
        },
    );
    result
}

/// Run the widget's query and map the result to a [`WidgetData`]. Kept
/// separate from the cache so it's unit-testable with a bare pool.
pub async fn compute(pool: &sqlx::SqlitePool, widget: &SqlWidget) -> Result<WidgetData, String> {
    let res = run_read_only(pool, &widget.query, VIEW_ROW_LIMIT)
        .await
        .map_err(|e: ReadOnlyError| e.to_string())?;
    map_widget_data(&widget.render, &res.columns, &res.rows)
}

/// Locate a named channel column in the result, or a clear config error naming
/// what's available (the operator authors both the query and the render, so a
/// mismatch is theirs to fix).
fn col_index(columns: &[String], name: &str) -> Result<usize, String> {
    columns.iter().position(|c| c == name).ok_or_else(|| {
        format!(
            "render references column '{name}' which the query does not return (available: {})",
            columns.join(", ")
        )
    })
}

/// Best-effort numeric read of a cell: a JSON number as-is, or a numeric
/// string (SQLite may hand back `count(*)` as text under some collations).
/// Non-numeric ⇒ `None`.
fn cell_num(v: &JsonValue) -> Option<f64> {
    match v {
        JsonValue::Number(n) => n.as_f64(),
        JsonValue::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Numeric read rounded to an integer for the count-style widgets.
fn cell_int(v: &JsonValue) -> i64 {
    cell_num(v).map(|f| f.round() as i64).unwrap_or(0)
}

/// A display label for a categorical cell — strings verbatim, numbers/bools
/// stringified, null rendered as an em dash so a NULL group is visible.
fn cell_label(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        JsonValue::Null => "—".to_string(),
        other => other.to_string(),
    }
}

/// Safe cell access. A sqlx result row always has exactly `columns.len()`
/// cells and every `col_index` is bounds-checked, so this never actually
/// falls through — but reading `row.get(i)` instead of `row[i]` keeps a
/// future refactor from turning a short row into a panic (Gemini). Out of
/// range ⇒ JSON null (the same "no value" the renderers already handle).
fn at(row: &[JsonValue], i: usize) -> &JsonValue {
    const NULL: JsonValue = JsonValue::Null;
    row.get(i).unwrap_or(&NULL)
}

/// The core render mapping: SQL `columns × rows` → a [`WidgetData`] per the
/// [`RenderSpec`]'s `kind` and channel columns. All column lookups are by name
/// against the result header, so a typo is a clear error, never a silent wrong
/// column.
pub fn map_widget_data(
    render: &RenderSpec,
    columns: &[String],
    rows: &[Vec<JsonValue>],
) -> Result<WidgetData, String> {
    match render.kind {
        RenderKind::Unknown => Err("render.kind is not a known value".to_string()),
        RenderKind::Table => {
            // Selected columns (or all), resolved to result indices once.
            let selected: Vec<(usize, String)> = match &render.columns {
                Some(cols) => cols
                    .iter()
                    .map(|name| {
                        let i = col_index(columns, name)?;
                        // Apply the optional header relabel.
                        let header = render
                            .labels
                            .as_ref()
                            .and_then(|m| m.get(name))
                            .cloned()
                            .unwrap_or_else(|| name.clone());
                        Ok((i, header))
                    })
                    .collect::<Result<_, String>>()?,
                None => columns
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let header = render
                            .labels
                            .as_ref()
                            .and_then(|m| m.get(name))
                            .cloned()
                            .unwrap_or_else(|| name.clone());
                        (i, header)
                    })
                    .collect(),
            };
            let headers: Vec<String> = selected.iter().map(|(_, h)| h.clone()).collect();
            let out_rows: Vec<Vec<JsonValue>> = rows
                .iter()
                .map(|row| selected.iter().map(|(i, _)| at(row, *i).clone()).collect())
                .collect();
            Ok(WidgetData::Table {
                columns: headers,
                rows: out_rows,
            })
        }
        RenderKind::Stat => {
            let vi = col_index(columns, require(&render.value, "value")?)?;
            // First row's value cell; an empty result reads as 0.
            let value = rows.first().map(|r| cell_int(at(r, vi))).unwrap_or(0);
            Ok(WidgetData::Stat {
                value,
                est_minutes: None,
            })
        }
        RenderKind::Bar => Ok(WidgetData::Bar {
            rows: bar_rows(render, columns, rows)?,
        }),
        RenderKind::Pie => Ok(WidgetData::Pie {
            rows: bar_rows(render, columns, rows)?,
            donut: render.donut.unwrap_or(false),
        }),
        RenderKind::Gauge => {
            let first = rows.first();
            let (total, active, ratio) = if let Some(value) = render.value.as_deref() {
                // Precomputed ratio in [0,1].
                let vi = col_index(columns, value)?;
                let r = first.and_then(|row| cell_num(at(row, vi))).unwrap_or(0.0);
                (0, 0, r)
            } else {
                // num / den pair.
                let ni = col_index(columns, require(&render.num, "num")?)?;
                let di = col_index(columns, require(&render.den, "den")?)?;
                let active = first.map(|row| cell_int(at(row, ni))).unwrap_or(0);
                let total = first.map(|row| cell_int(at(row, di))).unwrap_or(0);
                let ratio = if total > 0 {
                    active as f64 / total as f64
                } else {
                    0.0
                };
                (total, active, ratio)
            };
            Ok(WidgetData::Gauge {
                total,
                active,
                ratio,
                est_minutes: None,
                first: None,
                last: None,
            })
        }
    }
}

/// Shared `bar`/`pie` mapping: each result row → `{label, value}`. When
/// `render.limit` is set it's a **top-N by value** cap (the documented
/// contract), so we sort by value descending before truncating; without a
/// limit the query's own order is preserved (rank with `ORDER BY` in SQL).
fn bar_rows(
    render: &RenderSpec,
    columns: &[String],
    rows: &[Vec<JsonValue>],
) -> Result<Vec<BarRow>, String> {
    let li = col_index(columns, require(&render.label, "label")?)?;
    let vi = col_index(columns, require(&render.value, "value")?)?;
    let mut out: Vec<BarRow> = rows
        .iter()
        .map(|row| BarRow {
            label: cell_label(at(row, li)),
            value: cell_int(at(row, vi)),
            est_minutes: None,
        })
        .collect();
    if let Some(limit) = render.limit {
        // Stable sort by descending value (`Reverse` key) so equal-value rows
        // keep their query order — a deterministic tie-break — then keep top N.
        out.sort_by_key(|r| std::cmp::Reverse(r.value));
        out.truncate(limit as usize);
    }
    Ok(out)
}

/// Unwrap a required render channel or a clear error (validation should have
/// caught this at `view create`, but the mapper is also called for ad-hoc
/// widgets and defends itself).
fn require<'a>(field: &'a Option<String>, name: &str) -> Result<&'a str, String> {
    field
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("render.{name} is required for this kind"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn render(yaml: &str) -> RenderSpec {
        serde_yaml::from_str(yaml).expect("render parse")
    }

    #[test]
    fn table_all_columns_and_relabel() {
        let columns = cols(&["pc_id", "cves"]);
        let rows = vec![
            vec![serde_json::json!("pc-1"), serde_json::json!(3)],
            vec![serde_json::json!("pc-2"), serde_json::json!(1)],
        ];
        // All columns, one relabelled.
        let wd = map_widget_data(
            &render("{ kind: table, labels: { cves: CVE count } }"),
            &columns,
            &rows,
        )
        .unwrap();
        match wd {
            WidgetData::Table { columns, rows } => {
                assert_eq!(columns, vec!["pc_id".to_string(), "CVE count".to_string()]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], serde_json::json!("pc-1"));
            }
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn table_selected_columns_reorder() {
        let columns = cols(&["a", "b", "c"]);
        let rows = vec![vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3),
        ]];
        let wd =
            map_widget_data(&render("{ kind: table, columns: [c, a] }"), &columns, &rows).unwrap();
        match wd {
            WidgetData::Table { columns, rows } => {
                assert_eq!(columns, vec!["c".to_string(), "a".to_string()]);
                assert_eq!(rows[0], vec![serde_json::json!(3), serde_json::json!(1)]);
            }
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn missing_column_is_error() {
        let columns = cols(&["a"]);
        let rows = vec![vec![serde_json::json!(1)]];
        let err =
            map_widget_data(&render("{ kind: stat, value: nope }"), &columns, &rows).unwrap_err();
        assert!(err.contains("column 'nope'"), "err: {err}");
    }

    #[test]
    fn stat_reads_first_row() {
        let columns = cols(&["n"]);
        let rows = vec![vec![serde_json::json!(42)]];
        match map_widget_data(&render("{ kind: stat, value: n }"), &columns, &rows).unwrap() {
            WidgetData::Stat { value, .. } => assert_eq!(value, 42),
            _ => panic!("expected stat"),
        }
        // Empty result → 0, not an error.
        match map_widget_data(&render("{ kind: stat, value: n }"), &columns, &[]).unwrap() {
            WidgetData::Stat { value, .. } => assert_eq!(value, 0),
            _ => panic!("expected stat"),
        }
    }

    #[test]
    fn bar_and_pie_map_rows_with_limit() {
        let columns = cols(&["host", "cnt"]);
        // Deliberately NOT in value order, to prove `limit` sorts top-N by value.
        let rows = vec![
            vec![serde_json::json!("b"), serde_json::json!(5)],
            vec![serde_json::json!("a"), serde_json::json!(9)],
            vec![serde_json::json!("c"), serde_json::json!(2)],
        ];
        match map_widget_data(
            &render("{ kind: bar, label: host, value: cnt, limit: 2 }"),
            &columns,
            &rows,
        )
        .unwrap()
        {
            WidgetData::Bar { rows } => {
                assert_eq!(rows.len(), 2, "top-N by value");
                assert_eq!(rows[0].label, "a", "highest value first");
                assert_eq!(rows[0].value, 9);
                assert_eq!(rows[1].label, "b");
            }
            _ => panic!("expected bar"),
        }
        match map_widget_data(
            &render("{ kind: pie, label: host, value: cnt, donut: true }"),
            &columns,
            &rows,
        )
        .unwrap()
        {
            WidgetData::Pie { rows, donut } => {
                assert!(donut);
                assert_eq!(rows.len(), 3);
            }
            _ => panic!("expected pie"),
        }
    }

    #[test]
    fn gauge_value_and_num_den() {
        let columns = cols(&["affected", "total", "ratio"]);
        let rows = vec![vec![
            serde_json::json!(3),
            serde_json::json!(12),
            serde_json::json!(0.25),
        ]];
        // num/den form.
        match map_widget_data(
            &render("{ kind: gauge, num: affected, den: total }"),
            &columns,
            &rows,
        )
        .unwrap()
        {
            WidgetData::Gauge {
                total,
                active,
                ratio,
                ..
            } => {
                assert_eq!((total, active), (12, 3));
                assert!((ratio - 0.25).abs() < 1e-9);
            }
            _ => panic!("expected gauge"),
        }
        // value form.
        match map_widget_data(&render("{ kind: gauge, value: ratio }"), &columns, &rows).unwrap() {
            WidgetData::Gauge { ratio, .. } => assert!((ratio - 0.25).abs() < 1e-9),
            _ => panic!("expected gauge"),
        }
    }

    #[test]
    fn label_cell_handles_null_and_numbers() {
        assert_eq!(cell_label(&serde_json::json!("x")), "x");
        assert_eq!(cell_label(&serde_json::json!(7)), "7");
        assert_eq!(cell_label(&JsonValue::Null), "—");
    }

    #[tokio::test]
    async fn compute_runs_sql_end_to_end() {
        // Full path: run_read_only (validate + describe + stream) → render map.
        use sqlx::sqlite::SqlitePoolOptions;
        // `sqlite::memory:` is connection-local — pin the pool to one
        // connection so setup + query hit the same database (CodeRabbit).
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (host TEXT, n INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES ('a', 3), ('b', 1)")
            .execute(&pool)
            .await
            .unwrap();
        let w: SqlWidget = serde_yaml::from_str(
            "title: T
query: \"SELECT host, n FROM t ORDER BY n DESC\"
render: { kind: bar, label: host, value: n }
placement: { analytics: X }
",
        )
        .unwrap();
        match compute(&pool, &w).await.unwrap() {
            WidgetData::Bar { rows } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].label, "a");
                assert_eq!(rows[0].value, 3);
            }
            other => panic!("expected bar, got {other:?}"),
        }

        // A write query is rejected before it can run (defence in depth).
        let bad: SqlWidget = serde_yaml::from_str(
            "title: T
query: \"DELETE FROM t\"
render: { kind: table }
placement: { analytics: X }
",
        )
        .unwrap();
        let err = compute(&pool, &bad).await.unwrap_err();
        assert!(err.to_lowercase().contains("read-only"), "err: {err}");
    }
}
