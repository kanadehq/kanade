-- #390: normalize DEFAULT CURRENT_TIMESTAMP text to RFC 3339.
--
-- SQLite's CURRENT_TIMESTAMP stores 'YYYY-MM-DD HH:MM:SS' (space
-- separator, no offset), while every chrono-bound write and every
-- chrono-bound query parameter uses RFC 3339
-- ('YYYY-MM-DDTHH:MM:SS.fff+00:00'). TEXT comparison is
-- lexicographic and ' ' (0x20) < 'T' (0x54), so mixing the two
-- degrades range filters to UTC-date granularity: the Activity
-- page's "last 24h" only showed rows recorded since the current
-- UTC midnight (= 09:00 JST).
--
-- Rewrite the space-format values of every column that either gets
-- compared against a chrono bind (`recorded_at`, `observed_at`,
-- `initiated_at`) or shares a column with chrono-bound writers
-- (`finished_at` — reaped rows used CURRENT_TIMESTAMP; the
-- companion code change makes all of these writers bind explicit
-- RFC 3339 timestamps so the DEFAULT never fires again).
-- The `LIKE '____-__-__ %'` guard keeps already-RFC3339 values (and
-- NULLs) untouched, so the migration is idempotent in spirit.
--
-- strftime('%Y-%m-%dT%H:%M:%f', col) renders '...T HH:MM:SS.000';
-- appending '+00:00' matches the shape sqlx encodes for
-- chrono::DateTime<Utc> (CURRENT_TIMESTAMP is UTC, so the offset is
-- correct, not just cosmetic).

UPDATE execution_results
   SET recorded_at = strftime('%Y-%m-%dT%H:%M:%f', recorded_at) || '+00:00'
 WHERE recorded_at LIKE '____-__-__ %';

UPDATE execution_results
   SET finished_at = strftime('%Y-%m-%dT%H:%M:%f', finished_at) || '+00:00'
 WHERE finished_at LIKE '____-__-__ %';

UPDATE executions
   SET initiated_at = strftime('%Y-%m-%dT%H:%M:%f', initiated_at) || '+00:00'
 WHERE initiated_at LIKE '____-__-__ %';

UPDATE inventory_history
   SET observed_at = strftime('%Y-%m-%dT%H:%M:%f', observed_at) || '+00:00'
 WHERE observed_at LIKE '____-__-__ %';

UPDATE inventory_facts
   SET recorded_at = strftime('%Y-%m-%dT%H:%M:%f', recorded_at) || '+00:00'
 WHERE recorded_at LIKE '____-__-__ %';
