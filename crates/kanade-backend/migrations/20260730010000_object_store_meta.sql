-- #1216: display metadata index for the four NATS Object Store
-- buckets (collections / agent_releases / app_packages / scripts).
-- The list endpoints used to pay a full-stream scan per cold request
-- (ObjectStore::list() = DeliverPolicy::All over $O.<bucket>.M.>);
-- they now read this table, kept in sync by projector::object_meta's
-- per-bucket watch_with_history(). One row per live object; deletes
-- arrive as tombstone metas and remove the row.
CREATE TABLE object_store_meta (
    bucket   TEXT NOT NULL,
    key      TEXT NOT NULL,
    size     INTEGER NOT NULL,
    digest   TEXT,
    modified TEXT,  -- RFC3339, from ObjectInfo.mtime
    PRIMARY KEY (bucket, key)
);
