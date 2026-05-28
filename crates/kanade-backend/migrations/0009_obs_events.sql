-- Issue #246 — per-PC observability events table. The obs_events
-- projector appends to this table on every `obs.<pc_id>` JetStream
-- delivery; the SPA Events page queries it via /api/obs_events
-- routes. See `wire::ObsEvent` for the shape of each row.
--
-- Storage shape:
--   * pc_id / source / event_record_id — UNIQUE so agent
--                resends (under outbox replay, watermark drift,
--                JetStream redelivery) coalesce to one row. The
--                projector uses `INSERT OR IGNORE` against this
--                constraint to make resends harmless.
--   * `at` is the source's wall-clock instant (e.g. Windows
--                Event Log `TimeCreated`), NOT the moment the agent
--                or backend received the message. Timeline order
--                reflects when things happened on the box, not when
--                the projector heard about them.
--   * `payload` is TEXT holding a JSON document — varies per
--                `kind` (logon: {user, logon_type}; boot: null /
--                {}; diagnostic: {bucket, key}; etc.). Backend
--                stores opaquely, SPA renders per-kind.
--   * `event_record_id` is nullable because agent-emitted
--                milestones (e.g. agent_started) have no natural
--                unique id — the UNIQUE constraint then collapses
--                to (pc_id, source) which still rejects exact
--                replays but lets new emissions through.
--
-- Retention: 90 days. Cleanup loop (cleanup.rs) prunes rows older
-- than that on its 5 min cadence. At ~50 events/day/PC × 90 d ×
-- 1000 PCs = ~4.5 M rows, well within SQLite limits. The 90 d
-- window also matches the spec §2.3.1 retention for inventory
-- snapshots, so operators have one consistent mental "how far
-- back can I look" answer across the timeline surfaces.

CREATE TABLE obs_events (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    pc_id             TEXT NOT NULL,
    at                TIMESTAMP NOT NULL,
    kind              TEXT NOT NULL,
    source            TEXT NOT NULL,
    event_record_id   TEXT,
    payload           TEXT,
    UNIQUE(pc_id, source, event_record_id)
);

-- Per-PC timeline queries (the SPA's primary access pattern) walk
-- by (pc_id, at DESC). The natural row order from AUTOINCREMENT
-- can't serve this efficiently — explicit index required.
CREATE INDEX idx_obs_events_pc_at ON obs_events(pc_id, at DESC);

-- Filter-by-kind queries ("show me all logon events fleet-wide
-- today") walk by (kind, at DESC). Cheap secondary index on a
-- short string column, well worth the write cost.
CREATE INDEX idx_obs_events_kind_at ON obs_events(kind, at DESC);
