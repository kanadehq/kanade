//! #vuln-roadmap: global feed-table maintenance for the `feed:` manifest
//! hint. Unlike inventory [`explode`](super::explode) — which flattens a
//! JSON array into a per-`(pc_id, job_id)` derived table — a feed is GLOBAL
//! fleet-wide reference data (a vulnerability catalog, an EOL table): a
//! controller-tier job fetches it and the projector REPLACES that feed's
//! rows wholesale in the shared, fixed-schema `feeds` table keyed
//! `(feed_id, item_id)`. The whole element JSON goes into a `data` column so
//! a `view:` SQL can `json_extract` whatever shape the feed carries.
//!
//! No dynamic DDL here (the `feeds` table ships in a migration), so there's
//! no identifier-splicing surface: `feed_id` / `item_id` are bound *values*,
//! not spliced identifiers.

use anyhow::{Result, anyhow};
use kanade_shared::manifest::FeedSpec;
use serde_json::Value as JsonValue;
use sqlx::{Sqlite, SqlitePool, Transaction};
use tracing::{debug, warn};

/// Derive a stable `item_id` from an element's primary_key field(s). Returns
/// `None` when any key field is missing or JSON-null — such an element has no
/// stable identity and is skipped rather than keyed under a partial/blank id.
///
/// A single key is used verbatim (self-unambiguous, and readable — the common
/// `[cveID]` case). A composite key is **length-prefixed** (`"<byte-len>:<value>"`
/// concatenated) rather than separator-joined: any fixed separator (even a
/// control char) could appear inside an external feed's values and make
/// `["a", "b|c"]` collide with `["a|b", "c"]`. The length prefix pins each
/// boundary, so distinct tuples can never collapse to one id (CodeRabbit).
fn item_id_for(element: &JsonValue, primary_key: &[String]) -> Option<String> {
    let mut parts = Vec::with_capacity(primary_key.len());
    for key in primary_key {
        let part = match element.get(key) {
            Some(JsonValue::String(s)) => s.clone(),
            Some(JsonValue::Null) | None => return None,
            // Numbers / bools stringify deterministically; an object/array
            // key is unusual but `to_string` still gives a stable form.
            Some(other) => other.to_string(),
        };
        parts.push(part);
    }
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => {
            let mut out = String::new();
            for part in &parts {
                out.push_str(&part.len().to_string());
                out.push(':');
                out.push_str(part);
            }
            Some(out)
        }
    }
}

/// Replace all rows for `spec.id` in the shared `feeds` table with the
/// elements of `payload[spec.field]`. Transactional DELETE-then-INSERT so a
/// concurrent reader always sees a coherent snapshot of the feed.
///
/// Recency guard: the incoming `recorded_at` (the message's JetStream
/// publish time) must be strictly newer than the feed's stored watermark,
/// else the replace is skipped — a stale redelivery would otherwise wipe
/// newer reference data back to an older fetch, and an *exact* redelivery
/// (equal timestamp) would re-churn the whole catalog for no change. The
/// watermark lives in `feed_meta`, NOT `MAX(feeds.recorded_at)`, so an empty
/// refresh (which deletes every `feeds` row) still advances recency and can't
/// be undone by a later stale replay. Safe as a read-then-write because the
/// results consumer is serial (durable pull, one message at a time).
///
/// Returns the number of rows inserted (0 when the field is absent — the
/// feed's rows are cleared so a now-empty feed leaves no ghosts).
pub async fn replace_feed_rows(
    pool: &SqlitePool,
    spec: &FeedSpec,
    fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    recorded_at: chrono::DateTime<chrono::Utc>,
    payload: &JsonValue,
) -> Result<usize> {
    // The stored partition key. `id` is operator-authored YAML, so normalise
    // leading/trailing whitespace once here (validation only trims for its
    // emptiness/uniqueness checks) — otherwise `id: " cisa-kev"` would store
    // rows under a `feed_id` a `view:` query for `"cisa-kev"` never matches
    // (claude review). item_id values are feed DATA, not config, so they're
    // stored verbatim to match `json_extract(data, ...)`.
    let feed_id = spec.id.trim();

    // Recency guard — read the per-feed watermark from `feed_meta`. Using a
    // dedicated watermark (not `MAX(feeds.recorded_at)`) means a legitimate
    // empty refresh, which deletes every `feeds` row, still leaves a
    // watermark so a later stale redelivery can't resurrect old rows
    // (CodeRabbit). `<=` (not `<`): an exact redelivery carries the same
    // publish time as the stored watermark, so re-running the DELETE+INSERT
    // of a potentially large catalog is pure churn — skip it (Gemini review).
    let prior_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT recorded_at FROM feed_meta WHERE feed_id = ?")
            .bind(feed_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow!("read feed_meta watermark for {feed_id}: {e}"))?;
    if let Some(prior_at) = prior_at {
        if recorded_at <= prior_at {
            debug!(
                feed_id,
                "stale feed replay rejected by recency guard; skipping"
            );
            return Ok(0);
        }
    }

    // Defence in depth: a non-object payload (`[...]`, a bare string) makes
    // every `payload.get(field)` return `None`, which would silently wipe the
    // feed. Treat a wrong top-level shape as an error — the feed keeps its
    // prior rows and the operator sees the script bug (Gemini review) — rather
    // than a data-loss clear that looks like a successful empty refresh.
    if !payload.is_object() {
        return Err(anyhow!(
            "feed '{feed_id}' payload is not a JSON object (got {})",
            payload_kind(payload)
        ));
    }

    // Distinguish "field absent" from "field present but not an array". A
    // genuinely absent field means the job stopped producing this feed → clear
    // it (mirrors explode's "missing field clears state"). A present-but-wrong
    // shape (`{"field": {}}`) is a producer/schema bug → error and keep the
    // last-good rows rather than silently wiping them (CodeRabbit).
    let arr: &[JsonValue] = match payload.get(&spec.field) {
        None => &[],
        Some(JsonValue::Array(a)) => a,
        Some(other) => {
            return Err(anyhow!(
                "feed '{feed_id}' field '{}' is not an array (got {})",
                spec.field,
                payload_kind(other)
            ));
        }
    };

    let mut tx: Transaction<'_, Sqlite> = pool.begin().await?;
    // Wholesale replace: clear this feed's partition, then re-insert.
    sqlx::query("DELETE FROM feeds WHERE feed_id = ?")
        .bind(feed_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("delete prior feed rows for {feed_id}: {e}"))?;

    let mut inserted = 0;
    for element in arr {
        let Some(item_id) = item_id_for(element, &spec.primary_key) else {
            warn!(feed_id, "feed: skip element missing primary_key field(s)");
            continue;
        };
        // Store the compact JSON of the whole element — `view:` reads it via
        // json_extract, so we keep every field the feed carried. `INSERT OR
        // REPLACE`: two elements sharing a primary_key are "the same item", so
        // last-wins rather than a PK-collision error. Any OTHER insert error
        // (SQLITE_FULL / BUSY) propagates with `?` so the transaction rolls
        // back instead of committing a partial feed (Gemini review).
        let data = element.to_string();
        sqlx::query(
            "INSERT OR REPLACE INTO feeds (feed_id, item_id, data, fetched_at, recorded_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(feed_id)
        .bind(&item_id)
        .bind(&data)
        .bind(fetched_at)
        .bind(recorded_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("insert feed row {feed_id}/{item_id}: {e}"))?;
        inserted += 1;
    }

    // Advance the watermark in the SAME transaction so recency is correct even
    // for a zero-row refresh, and so a crash between the row write and the
    // watermark write can't leave them inconsistent. `item_count` /
    // `fetched_at` also give the dashboard a cheap "last refreshed, N items".
    sqlx::query(
        "INSERT INTO feed_meta (feed_id, recorded_at, fetched_at, item_count) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(feed_id) DO UPDATE SET \
             recorded_at = excluded.recorded_at, \
             fetched_at  = excluded.fetched_at, \
             item_count  = excluded.item_count",
    )
    .bind(feed_id)
    .bind(recorded_at)
    .bind(fetched_at)
    .bind(inserted as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| anyhow!("upsert feed_meta watermark for {feed_id}: {e}"))?;

    tx.commit().await?;
    debug!(feed_id, rows = inserted, "feed: rows refreshed");
    Ok(inserted)
}

/// Human-readable JSON kind for a shape-mismatch error.
fn payload_kind(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_pool() -> SqlitePool {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE feeds (
                feed_id TEXT NOT NULL, item_id TEXT NOT NULL, data TEXT NOT NULL,
                fetched_at TIMESTAMP, recorded_at TIMESTAMP NOT NULL,
                PRIMARY KEY (feed_id, item_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE feed_meta (
                feed_id TEXT PRIMARY KEY, recorded_at TIMESTAMP NOT NULL,
                fetched_at TIMESTAMP, item_count INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn kev_spec() -> FeedSpec {
        FeedSpec {
            id: "cisa-kev".into(),
            field: "vulnerabilities".into(),
            primary_key: vec!["cveID".into()],
        }
    }

    #[test]
    fn item_id_single_and_composite() {
        let el = serde_json::json!({"cveID": "CVE-2024-1", "vendor": "acme"});
        // Single key: verbatim value.
        assert_eq!(
            item_id_for(&el, &["cveID".into()]),
            Some("CVE-2024-1".to_string())
        );
        // Composite key: length-prefixed ("<len>:<value>" concatenated).
        assert_eq!(
            item_id_for(&el, &["vendor".into(), "cveID".into()]),
            Some("4:acme10:CVE-2024-1".to_string())
        );
        // Missing key field → no identity.
        assert_eq!(item_id_for(&el, &["missing".into()]), None);
    }

    #[test]
    fn item_id_composite_is_collision_free() {
        // A plain separator-join would collapse these two distinct tuples; the
        // length prefix keeps them apart no matter what the values contain.
        let a = serde_json::json!({"x": "a", "y": "b:c"});
        let b = serde_json::json!({"x": "a:b", "y": "c"});
        let pk = vec!["x".to_string(), "y".to_string()];
        assert_ne!(item_id_for(&a, &pk), item_id_for(&b, &pk));
    }

    #[tokio::test]
    async fn replace_is_wholesale_and_keyed() {
        let pool = mem_pool().await;
        let spec = kev_spec();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let payload = serde_json::json!({
            "vulnerabilities": [
                {"cveID": "CVE-2024-1", "product": "Chrome"},
                {"cveID": "CVE-2024-2", "product": "Firefox"},
            ]
        });
        let n = replace_feed_rows(&pool, &spec, None, t1, &payload)
            .await
            .unwrap();
        assert_eq!(n, 2);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM feeds")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2);

        // A newer fetch REPLACES the partition (not append).
        let t2 = t1 + chrono::Duration::hours(1);
        let payload2 = serde_json::json!({
            "vulnerabilities": [{"cveID": "CVE-2024-9", "product": "Edge"}]
        });
        let n = replace_feed_rows(&pool, &spec, None, t2, &payload2)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let ids: Vec<(String,)> = sqlx::query_as("SELECT item_id FROM feeds ORDER BY item_id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, vec![("CVE-2024-9".to_string(),)]);

        // json_extract on the stored `data` works (the `view:` read path).
        let product: (String,) =
            sqlx::query_as("SELECT json_extract(data, '$.product') FROM feeds")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(product.0, "Edge");
    }

    #[tokio::test]
    async fn stale_replay_rejected() {
        let pool = mem_pool().await;
        let spec = kev_spec();
        let t2 = chrono::DateTime::parse_from_rfc3339("2026-07-01T02:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let t1 = t2 - chrono::Duration::hours(1);

        replace_feed_rows(
            &pool,
            &spec,
            None,
            t2,
            &serde_json::json!({"vulnerabilities": [{"cveID": "NEW"}]}),
        )
        .await
        .unwrap();

        // An older redelivery must NOT wipe the newer partition.
        let n = replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [{"cveID": "OLD"}]}),
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        let ids: Vec<(String,)> = sqlx::query_as("SELECT item_id FROM feeds")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, vec![("NEW".to_string(),)]);
    }

    #[tokio::test]
    async fn missing_field_clears_feed() {
        let pool = mem_pool().await;
        let spec = kev_spec();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [{"cveID": "X"}]}),
        )
        .await
        .unwrap();

        // A later result without the field clears the feed's rows.
        let t2 = t1 + chrono::Duration::hours(1);
        let n = replace_feed_rows(&pool, &spec, None, t2, &serde_json::json!({"other": 1}))
            .await
            .unwrap();
        assert_eq!(n, 0);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM feeds")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0);
    }

    #[tokio::test]
    async fn empty_refresh_preserves_watermark_against_stale_replay() {
        // CodeRabbit scenario: a legit empty refresh deletes every row, so a
        // MAX(feeds.recorded_at) watermark would read NULL — the `feed_meta`
        // watermark must still reject a later stale replay that would
        // otherwise resurrect old rows.
        let pool = mem_pool().await;
        let spec = kev_spec();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let t2 = t1 + chrono::Duration::hours(1);

        // Full at t1, then empty at t2 (field absent → all rows cleared).
        replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [{"cveID": "OLD"}]}),
        )
        .await
        .unwrap();
        replace_feed_rows(&pool, &spec, None, t2, &serde_json::json!({}))
            .await
            .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM feeds")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "feed emptied");

        // A stale redelivery of the t1 result must NOT resurrect the old row.
        let n = replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [{"cveID": "OLD"}]}),
        )
        .await
        .unwrap();
        assert_eq!(n, 0, "stale replay rejected via feed_meta watermark");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM feeds")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "no resurrection");
    }

    #[tokio::test]
    async fn non_array_field_errors_without_wiping() {
        // Field present but not an array (`{"vulnerabilities": {}}`) is a
        // producer bug, not a "stopped producing" signal — error, keep rows.
        let pool = mem_pool().await;
        let spec = kev_spec();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [{"cveID": "KEEP"}]}),
        )
        .await
        .unwrap();

        let t2 = t1 + chrono::Duration::hours(1);
        let err = replace_feed_rows(
            &pool,
            &spec,
            None,
            t2,
            &serde_json::json!({"vulnerabilities": {}}),
        )
        .await
        .expect_err("non-array field must error");
        assert!(err.to_string().contains("is not an array"), "err: {err}");
        let ids: Vec<(String,)> = sqlx::query_as("SELECT item_id FROM feeds")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, vec![("KEEP".to_string(),)], "rows preserved");
    }

    #[tokio::test]
    async fn duplicate_item_id_is_last_wins() {
        // Two elements sharing a primary_key are "the same item" — INSERT OR
        // REPLACE keeps the last, and the batch still commits (no PK-collision
        // error aborts the feed).
        let pool = mem_pool().await;
        let spec = kev_spec();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let n = replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [
                {"cveID": "DUP", "note": "first"},
                {"cveID": "DUP", "note": "second"},
            ]}),
        )
        .await
        .unwrap();
        assert_eq!(n, 2, "both rows counted");
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT item_id, json_extract(data, '$.note') FROM feeds")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![("DUP".to_string(), "second".to_string())]);
    }

    #[tokio::test]
    async fn non_object_payload_errors_without_wiping() {
        // A bare array payload (script forgot the `{ field: [...] }` wrapper)
        // must NOT be read as "field absent → clear the feed".
        let pool = mem_pool().await;
        let spec = kev_spec();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [{"cveID": "KEEP"}]}),
        )
        .await
        .unwrap();

        let t2 = t1 + chrono::Duration::hours(1);
        let err = replace_feed_rows(&pool, &spec, None, t2, &serde_json::json!([1, 2, 3]))
            .await
            .expect_err("non-object payload must error");
        assert!(err.to_string().contains("not a JSON object"), "err: {err}");
        // Prior rows survive — no silent data loss.
        let ids: Vec<(String,)> = sqlx::query_as("SELECT item_id FROM feeds")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, vec![("KEEP".to_string(),)]);
    }

    #[tokio::test]
    async fn feed_id_is_trimmed_for_storage() {
        // A whitespace-padded id in the manifest must store under the trimmed
        // partition a `view:` query expects.
        let pool = mem_pool().await;
        let spec = FeedSpec {
            id: "  cisa-kev  ".into(),
            field: "vulnerabilities".into(),
            primary_key: vec!["cveID".into()],
        };
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        replace_feed_rows(
            &pool,
            &spec,
            None,
            t1,
            &serde_json::json!({"vulnerabilities": [{"cveID": "X"}]}),
        )
        .await
        .unwrap();
        let feed_ids: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT feed_id FROM feeds")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(feed_ids, vec![("cisa-kev".to_string(),)]);
    }
}
