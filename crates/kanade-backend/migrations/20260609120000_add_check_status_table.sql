-- #290 PR-E: fleet-wide compliance projection for operator-defined
-- health checks. A job whose Manifest carries a `check:` hint (with
-- `fleet` != false) has its `status` / `detail` upserted here by the
-- results projector on every successful run, so the operator SPA can
-- show which PCs pass/fail each check WITHOUT the operator also writing
-- an `inventory:` block. One row per (pc_id, check_name) — the latest
-- status, not a time series (same snapshot shape as inventory_facts).
CREATE TABLE IF NOT EXISTS check_status (
    pc_id       TEXT NOT NULL,
    check_name  TEXT NOT NULL,
    status      TEXT NOT NULL,   -- ok / warn / fail / unknown
    detail      TEXT,
    recorded_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (pc_id, check_name)
);

-- `check_name`-leading index for the by-check queries. No separate
-- pc_id index: it's the leading column of the (pc_id, check_name) PK,
-- so `WHERE pc_id = ?` already uses the PK B-tree.
CREATE INDEX IF NOT EXISTS idx_check_status_name ON check_status(check_name);
