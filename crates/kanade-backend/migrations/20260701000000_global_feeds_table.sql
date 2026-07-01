-- #vuln-roadmap: global external-data feed store for the `feed:` manifest
-- hint. Unlike inventory `explode:` derived tables (keyed per (pc_id,
-- job_id)), a feed is fleet-wide REFERENCE data — a vulnerability catalog,
-- an EOL table, a license roster — fetched by a controller-tier job and
-- shared across every PC. One fixed-schema table holds every feed,
-- partitioned by `feed_id`; each item's full JSON lands in `data` so a
-- `view:` SQL can `json_extract` whatever shape the feed carries without a
-- per-feed schema or dynamic DDL.
--
-- Not NATS-replayable: a result projects the *current* fetch, so after a
-- projector wipe the table is empty until the feed job's next scheduled
-- run repopulates it (self-healing). The projector REPLACES a feed_id's
-- rows wholesale on every result.
-- The hot read pattern is "all items in one feed" (a `view:` joins a feed
-- partition against an inventory table). `WHERE feed_id = ?` is already
-- served by the composite PRIMARY KEY's index via its leftmost prefix, so no
-- separate `feed_id` index is needed (it would only add write overhead).
CREATE TABLE IF NOT EXISTS feeds (
    feed_id     TEXT NOT NULL,
    item_id     TEXT NOT NULL,
    data        TEXT NOT NULL,
    fetched_at  TIMESTAMP,
    recorded_at TIMESTAMP NOT NULL,
    PRIMARY KEY (feed_id, item_id)
);

-- Per-feed watermark + summary, one row per feed_id. The projector's
-- stale-replay guard reads `recorded_at` from HERE, not `MAX(feeds.recorded_at)`:
-- a legitimate empty refresh deletes every `feeds` row, so a MAX-over-rows
-- watermark would read NULL and let a later stale redelivery resurrect old
-- rows. This table persists the watermark independently of row presence, and
-- `item_count` / `fetched_at` double as cheap dashboard metadata ("KEV: 1500
-- items, refreshed 2h ago"). Updated in the same transaction as the rows.
CREATE TABLE IF NOT EXISTS feed_meta (
    feed_id     TEXT PRIMARY KEY,
    recorded_at TIMESTAMP NOT NULL,
    fetched_at  TIMESTAMP,
    item_count  INTEGER NOT NULL
);
