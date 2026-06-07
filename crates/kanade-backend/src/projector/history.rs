//! v0.31 / #41: change-only history projection.
//!
//! Pairs with the `explode` module — those derived tables are the
//! "what's installed now" snapshot, this module logs "what changed
//! since the previous scan". The projector diff fires BEFORE the
//! explode DELETE-then-INSERT replace so the comparison sees prior
//! state.
//!
//! Volume control: only writes when something actually changed.
//! No-op scans (most of them, once the fleet stabilises) produce
//! zero rows. Combined with the 90-day default retention sweeper
//! (see `cleanup` module), volume tracks fleet churn rather than
//! scan cadence.
//!
//! Identity: arrays are matched element-to-element using the
//! `spec.primary_key` tuple. `identity_json` on each change event
//! serialises just those columns so queries like "every PC ever
//! running Chrome" can filter without per-manifest schema.

use anyhow::Result;
use kanade_shared::manifest::ExplodeSpec;
use serde_json::Value as JsonValue;
use sqlx::{AssertSqlSafe, Sqlite, Transaction};
use std::collections::HashMap;

/// One change event ready to insert into `inventory_history`.
/// Constructed by [`diff_explode_rows`] (array-element history,
/// `identity_json = Some(...)`) or [`diff_scalars`] (scalar field
/// history, `identity_json = None`), consumed by [`write_events`].
///
/// `field_path` was carried in on the `write_events` signature
/// pre-#93 because every event in a single batch shared it
/// (one explode field per call). Scalar history mixes multiple
/// fields per call (one event per changed scalar), so field_path
/// moved onto the event itself; the explode path now stamps it
/// per element and write_events stays unchanged for both callers.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEvent {
    pub field_path: String,
    pub change_kind: &'static str, // "added" | "removed" | "changed"
    /// JSON object of the explode spec's primary_key tuple values
    /// for array-element history; `None` for scalar-field history
    /// (the field name in `field_path` is identity enough).
    pub identity_json: Option<String>,
    pub before_json: Option<String>,
    pub after_json: Option<String>,
}

/// Read the prior rows for `(pc_id, job_id)` from the derived
/// explode table and diff against the incoming `arr` (the payload's
/// `spec.field` array). Returns the list of change events to
/// persist. The diff is keyed on `spec.primary_key` — elements that
/// match a prior row are checked for column differences, unmatched
/// prior rows are `removed`, unmatched new elements are `added`.
pub async fn diff_explode_rows(
    tx: &mut Transaction<'_, Sqlite>,
    spec: &ExplodeSpec,
    pc_id: &str,
    job_id: &str,
    arr: &[JsonValue],
) -> Result<Vec<HistoryEvent>> {
    // Pull every prior row for this (pc_id, job_id) — we read into
    // a HashMap keyed by the primary-key tuple so the per-element
    // lookup is O(1). The select column list is built from the
    // spec so the test-time table shape stays in sync with the
    // manifest's declaration.
    let select_cols: Vec<String> = spec
        .columns
        .iter()
        .map(|c| format!("\"{}\"", c.field))
        .collect();
    let prior_sql = format!(
        "SELECT {} FROM \"{}\" WHERE pc_id = ? AND job_id = ?",
        select_cols.join(", "),
        spec.table,
    );
    let prior_rows: Vec<sqlx::sqlite::SqliteRow> = sqlx::query(AssertSqlSafe(prior_sql))
        .bind(pc_id)
        .bind(job_id)
        .fetch_all(&mut **tx)
        .await?;

    // Convert each row to a JSON object so subsequent comparisons
    // are uniform with the incoming `arr` elements.
    let mut prior_by_key: HashMap<String, JsonValue> = HashMap::with_capacity(prior_rows.len());
    for row in &prior_rows {
        let obj = row_to_json(row, spec);
        let key = identity_string(&obj, &spec.primary_key);
        prior_by_key.insert(key, obj);
    }

    // Walk incoming elements: each one either matches a prior row
    // (compare for `changed`) or is new (`added`). Track seen keys
    // so the remaining prior_by_key entries become `removed`.
    let mut events = Vec::new();
    let mut seen_keys: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(arr.len());

    for element in arr {
        let key = identity_string(element, &spec.primary_key);
        // CodeRabbit #86 fix: skip duplicate identities in the same
        // incoming payload. Pre-fix every duplicate generated an
        // additional `added` / `changed` event, but the explode
        // INSERT step drops the duplicate on PK conflict — so the
        // history could record an event that doesn't match the
        // final snapshot state. `HashSet::insert` returns false
        // when the value was already present, so the early-continue
        // collapses N duplicates to one history event.
        if !seen_keys.insert(key.clone()) {
            continue;
        }

        let identity_json = Some(identity_json_for(element, &spec.primary_key));
        match prior_by_key.get(&key) {
            None => events.push(HistoryEvent {
                field_path: spec.field.clone(),
                change_kind: "added",
                identity_json,
                before_json: None,
                after_json: Some(serde_json::to_string(element)?),
            }),
            Some(prior) if rows_differ(prior, element, spec) => events.push(HistoryEvent {
                field_path: spec.field.clone(),
                change_kind: "changed",
                identity_json,
                before_json: Some(serde_json::to_string(prior)?),
                after_json: Some(serde_json::to_string(element)?),
            }),
            Some(_) => { /* identical — no event */ }
        }
    }

    for (key, prior) in &prior_by_key {
        if seen_keys.contains(key) {
            continue;
        }
        events.push(HistoryEvent {
            field_path: spec.field.clone(),
            change_kind: "removed",
            identity_json: Some(identity_json_for(prior, &spec.primary_key)),
            before_json: Some(serde_json::to_string(prior)?),
            after_json: None,
        });
    }

    Ok(events)
}

/// v0.35 / #93: diff a manifest's top-level scalar fields against
/// the prior `inventory_facts.facts_json`. Called from `results.rs`
/// BEFORE the inventory_facts UPSERT overwrites the prior row, so
/// the comparison sees the actual previous values.
///
/// First-ever scan (`prior_facts_json = None`) emits one `added`
/// per scalar present in the new payload. Subsequent scans only
/// emit events for fields whose value changed — matching the
/// volume-control discipline the array-element history follows.
/// Same-value rescans produce zero events.
///
/// Values are wrapped as `{"value": <v>}` so the SPA's existing
/// diff renderer (#92 / #113) can lift them out the same way it
/// does column diffs in array history.
pub fn diff_scalars(
    prior_facts_json: Option<&str>,
    new_facts: &JsonValue,
    scalars: &[String],
) -> Result<Vec<HistoryEvent>> {
    let prior: Option<JsonValue> = prior_facts_json.map(serde_json::from_str).transpose()?;
    let mut events = Vec::new();
    for field in scalars {
        let prior_val = prior.as_ref().and_then(|p| p.get(field));
        let new_val = new_facts.get(field);
        match (prior_val, new_val) {
            // Field missing from new payload (rare — script chose to
            // drop it for this run). Don't emit a `removed` for
            // scalars; that's intentionally out of scope per the
            // proposal in #93. Operator can still see the last seen
            // value via the most recent `changed` event's after_json.
            (_, None) => {}
            // First-ever observation — emit `added`. We also fire
            // this when prior_facts_json existed but did not
            // include the field (manifest grew a new scalar between
            // scans); same outcome from the operator's perspective.
            (None, Some(v)) => {
                events.push(HistoryEvent {
                    field_path: field.clone(),
                    change_kind: "added",
                    identity_json: None,
                    before_json: None,
                    after_json: Some(serde_json::to_string(&serde_json::json!({ "value": v }))?),
                });
            }
            (Some(p), Some(n)) if p != n => {
                events.push(HistoryEvent {
                    field_path: field.clone(),
                    change_kind: "changed",
                    identity_json: None,
                    before_json: Some(serde_json::to_string(&serde_json::json!({ "value": p }))?),
                    after_json: Some(serde_json::to_string(&serde_json::json!({ "value": n }))?),
                });
            }
            _ => {}
        }
    }
    Ok(events)
}

/// Persist a batch of change events to `inventory_history` inside
/// the caller's transaction. Empty `events` is a no-op (typical
/// case for fleet-stable scans). `field_path` rides on each event
/// (v0.35 / #93) so a single call can mix multiple scalar fields
/// in one INSERT — array-element callers pass events that all
/// share the same `spec.field` and pay nothing for the per-event
/// stamp.
pub async fn write_events(
    tx: &mut Transaction<'_, Sqlite>,
    pc_id: &str,
    job_id: &str,
    events: &[HistoryEvent],
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    // Gemini #86 fix: single multi-VALUES insert via QueryBuilder
    // instead of N round-trips. SQLite likes batch INSERTs and the
    // transaction overhead per statement is non-trivial when a PC
    // turns over a hundred installed apps. Still inside the caller's
    // transaction for atomicity vs the DELETE-INSERT replace.
    // #390: observed_at is bound explicitly (RFC 3339, one shared
    // stamp per batch) — the column's DEFAULT CURRENT_TIMESTAMP
    // writes space-separated text that breaks lexicographic
    // `observed_at >= ?` filters on the timeline / first-seen APIs.
    let observed_at = chrono::Utc::now();
    let mut qb = sqlx::QueryBuilder::<Sqlite>::new(
        "INSERT INTO inventory_history (
             pc_id, job_id, field_path, identity_json,
             change_kind, before_json, after_json, observed_at
         ) ",
    );
    qb.push_values(events, |mut b, ev| {
        b.push_bind(pc_id)
            .push_bind(job_id)
            .push_bind(&ev.field_path)
            .push_bind(&ev.identity_json)
            .push_bind(ev.change_kind)
            .push_bind(&ev.before_json)
            .push_bind(&ev.after_json)
            .push_bind(observed_at);
    });
    qb.build().execute(&mut **tx).await?;
    Ok(())
}

/// Stable string key built from the primary_key tuple's values
/// inside one element. Used to match prior rows ↔ new elements
/// inside [`diff_explode_rows`]; sorted via the explicit pk order
/// so the same physical row produces the same string regardless
/// of JSON object iteration order.
fn identity_string(obj: &JsonValue, primary_key: &[String]) -> String {
    let mut parts = Vec::with_capacity(primary_key.len());
    for k in primary_key {
        let v = obj.get(k).cloned().unwrap_or(JsonValue::Null);
        parts.push(format!("{k}={v}"));
    }
    parts.join("|")
}

/// Serialise just the primary_key fields as a JSON object — the
/// payload for `inventory_history.identity_json`.
fn identity_json_for(obj: &JsonValue, primary_key: &[String]) -> String {
    let mut map = serde_json::Map::new();
    for k in primary_key {
        let v = obj.get(k).cloned().unwrap_or(JsonValue::Null);
        map.insert(k.clone(), v);
    }
    serde_json::Value::Object(map).to_string()
}

/// Compare two element-shaped JSON objects across every declared
/// column. Returns true if any non-key column differs. We don't
/// dive into nested arrays inside an element — that level of
/// granularity is rare and would need its own identity contract.
fn rows_differ(a: &JsonValue, b: &JsonValue, spec: &ExplodeSpec) -> bool {
    for col in &spec.columns {
        let av = a.get(&col.field);
        let bv = b.get(&col.field);
        if av != bv {
            return true;
        }
    }
    false
}

/// Convert a SqliteRow back to a serde JSON object using the
/// spec's column list + declared kinds. Used to bring prior rows
/// into a comparable shape for [`rows_differ`].
///
/// Gemini #86 fix: decode errors (schema drift, manifest typo
/// renaming a column, etc.) now warn-log instead of being silently
/// swallowed via `.ok().flatten()`. The value still falls back to
/// JsonValue::Null so the diff can continue — but the warning
/// gives operators a breadcrumb when "every scan produces a
/// changed event" turns out to be a column-decode bug, not real
/// data churn.
fn row_to_json(row: &sqlx::sqlite::SqliteRow, spec: &ExplodeSpec) -> JsonValue {
    use sqlx::Row;
    let mut map = serde_json::Map::new();
    for col in &spec.columns {
        let v: JsonValue = match col.kind.as_deref() {
            Some("integer") => match row.try_get::<Option<i64>, _>(col.field.as_str()) {
                Ok(Some(i)) => JsonValue::Number(i.into()),
                Ok(None) => JsonValue::Null,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        column = %col.field,
                        kind = "integer",
                        "history diff: row decode failed (treating as NULL — diff may generate a false 'changed' event)",
                    );
                    JsonValue::Null
                }
            },
            Some("real") => match row.try_get::<Option<f64>, _>(col.field.as_str()) {
                Ok(Some(f)) => serde_json::Number::from_f64(f)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null),
                Ok(None) => JsonValue::Null,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        column = %col.field,
                        kind = "real",
                        "history diff: row decode failed (treating as NULL)",
                    );
                    JsonValue::Null
                }
            },
            _ => match row.try_get::<Option<String>, _>(col.field.as_str()) {
                Ok(Some(s)) => JsonValue::String(s),
                Ok(None) => JsonValue::Null,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        column = %col.field,
                        kind = "text",
                        "history diff: row decode failed (treating as NULL)",
                    );
                    JsonValue::Null
                }
            },
        };
        map.insert(col.field.clone(), v);
    }
    JsonValue::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::manifest::{ExplodeColumn, ExplodeSpec};
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;

    fn sample_apps_spec() -> ExplodeSpec {
        ExplodeSpec {
            field: "apps".into(),
            table: "inventory_sw_apps".into(),
            primary_key: vec!["name".into(), "source".into()],
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
            ],
            track_history: true,
        }
    }

    async fn fresh_pool_with_table() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        crate::projector::explode::ensure_table(&pool, &sample_apps_spec())
            .await
            .unwrap();
        pool
    }

    async fn seed_row(pool: &SqlitePool, name: &str, source: &str, version: &str) {
        sqlx::query(
            "INSERT INTO inventory_sw_apps
                (pc_id, job_id, source, name, version)
             VALUES ('pc-1', 'inventory-sw', ?, ?, ?)",
        )
        .bind(source)
        .bind(name)
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn first_ever_scan_produces_added_events_for_every_element() {
        let pool = fresh_pool_with_table().await;
        let spec = sample_apps_spec();
        let arr = vec![
            serde_json::json!({"name": "Chrome", "source": "msi", "version": "120"}),
            serde_json::json!({"name": "Firefox", "source": "wow6432", "version": "122"}),
        ];
        let mut tx = pool.begin().await.unwrap();
        let events = diff_explode_rows(&mut tx, &spec, "pc-1", "inventory-sw", &arr)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.change_kind == "added"));
        assert!(events.iter().all(|e| e.before_json.is_none()));
        assert!(events.iter().all(|e| e.after_json.is_some()));
    }

    #[tokio::test]
    async fn stable_scan_produces_no_events() {
        let pool = fresh_pool_with_table().await;
        seed_row(&pool, "Chrome", "msi", "120").await;
        let spec = sample_apps_spec();
        let arr = vec![serde_json::json!({"name":"Chrome","source":"msi","version":"120"})];
        let mut tx = pool.begin().await.unwrap();
        let events = diff_explode_rows(&mut tx, &spec, "pc-1", "inventory-sw", &arr)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(
            events.is_empty(),
            "identical scans must produce zero events"
        );
    }

    #[tokio::test]
    async fn version_change_produces_changed_event_with_before_after() {
        let pool = fresh_pool_with_table().await;
        seed_row(&pool, "Chrome", "msi", "120").await;
        let spec = sample_apps_spec();
        let arr = vec![serde_json::json!({"name":"Chrome","source":"msi","version":"121"})];
        let mut tx = pool.begin().await.unwrap();
        let events = diff_explode_rows(&mut tx, &spec, "pc-1", "inventory-sw", &arr)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.change_kind, "changed");
        assert!(
            ev.before_json
                .as_ref()
                .unwrap()
                .contains("\"version\":\"120\"")
        );
        assert!(
            ev.after_json
                .as_ref()
                .unwrap()
                .contains("\"version\":\"121\"")
        );
        // identity_json carries the key tuple so cross-PC search can
        // filter on "this exact app" regardless of version.
        let identity = ev
            .identity_json
            .as_ref()
            .expect("explode events carry identity");
        assert!(identity.contains("\"name\":\"Chrome\""));
        assert!(identity.contains("\"source\":\"msi\""));
        assert_eq!(ev.field_path, "apps");
    }

    #[tokio::test]
    async fn uninstall_produces_removed_event() {
        let pool = fresh_pool_with_table().await;
        seed_row(&pool, "Chrome", "msi", "120").await;
        seed_row(&pool, "Firefox", "wow6432", "122").await;
        let spec = sample_apps_spec();
        // New scan only has Firefox — Chrome was uninstalled.
        let arr = vec![serde_json::json!({"name":"Firefox","source":"wow6432","version":"122"})];
        let mut tx = pool.begin().await.unwrap();
        let events = diff_explode_rows(&mut tx, &spec, "pc-1", "inventory-sw", &arr)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.change_kind, "removed");
        assert!(ev.after_json.is_none());
        assert!(
            ev.before_json
                .as_ref()
                .unwrap()
                .contains("\"name\":\"Chrome\"")
        );
    }

    #[tokio::test]
    async fn mixed_diff_emits_all_three_kinds() {
        let pool = fresh_pool_with_table().await;
        seed_row(&pool, "Chrome", "msi", "120").await; // will change
        seed_row(&pool, "Firefox", "wow6432", "122").await; // will be removed
        let spec = sample_apps_spec();
        let arr = vec![
            serde_json::json!({"name":"Chrome","source":"msi","version":"121"}), // changed
            serde_json::json!({"name":"Edge","source":"appx","version":"122"}),  // added
        ];
        let mut tx = pool.begin().await.unwrap();
        let events = diff_explode_rows(&mut tx, &spec, "pc-1", "inventory-sw", &arr)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(events.len(), 3);
        let kinds: std::collections::HashSet<_> = events.iter().map(|e| e.change_kind).collect();
        assert!(kinds.contains("added"));
        assert!(kinds.contains("removed"));
        assert!(kinds.contains("changed"));
    }

    #[tokio::test]
    async fn write_events_persists_to_inventory_history() {
        let pool = fresh_pool_with_table().await;
        let events = vec![HistoryEvent {
            field_path: "apps".into(),
            change_kind: "added",
            identity_json: Some(r#"{"name":"Chrome"}"#.into()),
            before_json: None,
            after_json: Some(r#"{"name":"Chrome","version":"120"}"#.into()),
        }];
        let mut tx = pool.begin().await.unwrap();
        write_events(&mut tx, "pc-1", "inventory-sw", &events)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM inventory_history")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
        let row: (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT pc_id, job_id, field_path, change_kind, before_json, after_json \
             FROM inventory_history",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "pc-1");
        assert_eq!(row.1, "inventory-sw");
        assert_eq!(row.2, "apps");
        assert_eq!(row.3, "added");
        assert_eq!(row.4, None);
        assert!(row.5.unwrap().contains("Chrome"));
    }

    // v0.35 / #93: scalar-field diff tests. No DB needed — diff_scalars
    // is pure logic over two JsonValues, the SQL surface is exercised
    // by the existing write_events test.

    #[test]
    fn diff_scalars_first_scan_emits_added_per_field() {
        let new_facts = serde_json::json!({
            "ram_bytes": 17179869184_u64,
            "os_version": "10.0.22631",
            "cpu_model": "Intel(R) Core(TM) i7-1165G7",
        });
        let events =
            diff_scalars(None, &new_facts, &["ram_bytes".into(), "os_version".into()]).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.change_kind == "added"));
        assert!(events.iter().all(|e| e.identity_json.is_none()));
        assert!(events.iter().all(|e| e.before_json.is_none()));
        assert!(events.iter().all(|e| e.after_json.is_some()));
        // field_path mirrors the scalar name so the SPA History tab
        // groups it under e.g. "ram_bytes" without any extra mapping.
        let paths: std::collections::HashSet<_> =
            events.iter().map(|e| e.field_path.as_str()).collect();
        assert!(paths.contains("ram_bytes"));
        assert!(paths.contains("os_version"));
    }

    #[test]
    fn diff_scalars_identical_rescan_emits_nothing() {
        let prior = r#"{"ram_bytes":17179869184,"os_version":"10.0.22631"}"#;
        let new_facts =
            serde_json::json!({"ram_bytes": 17179869184_u64, "os_version": "10.0.22631"});
        let events = diff_scalars(
            Some(prior),
            &new_facts,
            &["ram_bytes".into(), "os_version".into()],
        )
        .unwrap();
        assert!(events.is_empty(), "stable rescans must produce zero events");
    }

    #[test]
    fn diff_scalars_value_change_emits_changed_with_value_wrapper() {
        let prior = r#"{"os_version":"10.0.19045"}"#;
        let new_facts = serde_json::json!({"os_version": "10.0.22631"});
        let events = diff_scalars(Some(prior), &new_facts, &["os_version".into()]).unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.field_path, "os_version");
        assert_eq!(ev.change_kind, "changed");
        assert!(ev.identity_json.is_none());
        // The `{"value": <v>}` wrapper lets the SPA's diff renderer
        // (#92 / #113) extract before/after via the same code path
        // it uses on explode-row column diffs.
        assert!(
            ev.before_json
                .as_ref()
                .unwrap()
                .contains("\"value\":\"10.0.19045\"")
        );
        assert!(
            ev.after_json
                .as_ref()
                .unwrap()
                .contains("\"value\":\"10.0.22631\"")
        );
    }

    #[test]
    fn diff_scalars_field_missing_from_new_payload_no_event() {
        // Operator's manifest grew a `bios_version` scalar; the
        // script for THIS run forgot to emit it. We don't fabricate
        // a `removed` event — the value will reappear next scan when
        // the script is fixed, and a `removed` here would mislead.
        let prior = r#"{"bios_version":"1.30"}"#;
        let new_facts = serde_json::json!({"ram_bytes": 1024});
        let events = diff_scalars(Some(prior), &new_facts, &["bios_version".into()]).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn diff_scalars_added_when_prior_lacks_the_field() {
        // Manifest grew a new scalar between scans (operator
        // edited `history_scalars` and re-registered). Prior row
        // doesn't have the field → treat as first-ever observation.
        let prior = r#"{"ram_bytes":1024}"#;
        let new_facts = serde_json::json!({"ram_bytes": 1024, "os_version": "10.0.22631"});
        let events = diff_scalars(Some(prior), &new_facts, &["os_version".into()]).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].change_kind, "added");
        assert_eq!(events[0].field_path, "os_version");
    }
}
