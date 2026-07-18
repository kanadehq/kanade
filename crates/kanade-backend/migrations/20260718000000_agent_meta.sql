-- Query-side projection of the `agent_meta` NATS KV bucket (operator-
-- managed per-PC key/value metadata — #1045/#1046). The KV bucket stays
-- the source of truth (the SPA PUT and `kanade meta` write it); this
-- table is a read-only rebuild kept in sync by `projector::agent_meta`
-- (boot reconcile + live watch). It exists so the Agents list can show
-- metadata as columns and filter by it in SQL (#1051). The API never
-- writes here — writes go to KV.
--
-- On a `-WipeDb` upgrade this table is recreated empty and the projector's
-- boot reconcile repopulates it from KV, so no data is lost.
CREATE TABLE IF NOT EXISTS agent_meta (
    pc_id TEXT NOT NULL,
    key   TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (pc_id, key)
);

-- Drives the `?meta_key=&meta_value=` filter (WHERE key = ? AND value
-- LIKE ?) and the `SELECT DISTINCT key` column-picker feed.
CREATE INDEX IF NOT EXISTS idx_agent_meta_key_value ON agent_meta (key, value);
