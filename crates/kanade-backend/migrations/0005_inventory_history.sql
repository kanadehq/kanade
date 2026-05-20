-- v0.31 / #41: change-only history log for inventory facts.
-- Pairs with #40's derived `explode` tables — those answer "what's
-- installed now?", this answers "when did it appear / disappear /
-- change?". Storing snapshots would explode (3000 PCs × 150 apps
-- × 24 hourly scans/day ≈ 10M row-events/day); storing CHANGES
-- only keeps volume tied to actual fleet churn, not scan cadence.
--
-- Identity model: arrays are identified by a tuple declared on the
-- manifest's explode spec (`key:`). For `apps`, typical key is
-- `[name, source]`. The tuple is serialised as `identity_json` so
-- queries like "every event for Chrome from msi" can match without
-- a per-manifest schema. For scalar history (top-level fields,
-- future PR), identity_json is NULL and the field name lives in
-- `field_path`.
--
-- Retention: see `crates/kanade-backend/src/cleanup.rs` — sweeper
-- deletes rows older than 90 days by default. 90 d × per-PC churn
-- rate is the upper bound on storage; for typical fleets that's
-- well under 1 GB.

CREATE TABLE inventory_history (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    pc_id         TEXT NOT NULL,
    job_id        TEXT NOT NULL,
    field_path    TEXT NOT NULL,
    -- JSON object capturing the spec.key tuple values for array
    -- elements (e.g. {"name":"Chrome","source":"msi"}); NULL for
    -- scalar field changes (future scope — only array history in
    -- this migration's first consumer).
    identity_json TEXT,
    -- 'added' / 'removed' / 'changed'. CodeRabbit #86 fix: enforce
    -- the enum at the DB layer with a CHECK constraint so any non-
    -- projector writer (manual SQL, future migrations, a buggy
    -- backend in flight) can't sneak invalid values past the
    -- projector contract.
    change_kind   TEXT NOT NULL
        CHECK (change_kind IN ('added', 'removed', 'changed')),
    -- Full row snapshot as JSON. `before` is NULL on 'added';
    -- `after` is NULL on 'removed'; both populated on 'changed'.
    before_json   TEXT,
    after_json    TEXT,
    observed_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Per-PC timeline (newest first) — drives the upcoming SPA History
-- tab.
CREATE INDEX idx_inventory_history_pc
    ON inventory_history(pc_id, observed_at DESC);

-- Fleet-wide "when did X happen for any PC" — typical query for
-- the rollout-curve / first-seen surface.
CREATE INDEX idx_inventory_history_field
    ON inventory_history(job_id, field_path, observed_at DESC);

-- Identity-keyed lookup — "every PC ever installed Chrome from
-- msi". Restricted to job_id + field so the index stays narrow.
CREATE INDEX idx_inventory_history_identity
    ON inventory_history(job_id, field_path, identity_json);

-- Retention scan target — sweeper reads + deletes by observed_at,
-- this lets it find candidates without a table scan.
CREATE INDEX idx_inventory_history_observed_at
    ON inventory_history(observed_at);
