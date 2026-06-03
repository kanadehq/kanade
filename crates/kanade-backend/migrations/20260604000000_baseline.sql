-- 20260604000000 — squashed baseline (PoC reset).
--
-- Folds the previous 0001..0011 migrations into a single landing point.
-- The projector DB is a rebuildable *projection* of the JetStream
-- streams, so at PoC stage we wipe-and-recreate rather than chain the
-- migration history forward (same call the v0.16 0001_baseline made).
--
-- Naming: migrations now use a TIMESTAMP prefix (YYYYMMDDHHMMSS, as
-- `sqlx migrate add <name>` generates) instead of sequential 00NN, so
-- parallel PRs can never collide on the same sqlx version number — the
-- bug that broke main when two PRs both added 0010_*. See
-- migrations/README.md.
--
-- UPGRADE NOTE: an existing backend.db (with 0001..0011 recorded in
-- _sqlx_migrations) will fail sqlx's missing-migration check against
-- this squashed set. Delete the SQLite file and let the backend
-- re-create it; the data is derived from JetStream and re-projects.


CREATE TABLE IF NOT EXISTS agents (
    pc_id                    TEXT PRIMARY KEY,
    hostname                 TEXT,
    last_heartbeat           TIMESTAMP,
    agent_version            TEXT,
    os_family                TEXT,
    updated_at               TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Agent self-perf, added by the v0.37 agents_perf migration.
    agent_cpu_pct            REAL,
    agent_rss_bytes          INTEGER,
    agent_disk_read_bytes    INTEGER,
    agent_disk_written_bytes INTEGER
);

CREATE TABLE IF NOT EXISTS audit_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    actor        TEXT NOT NULL,
    action       TEXT NOT NULL,
    target       TEXT,
    payload      TEXT,
    occurred_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS execution_results (
    result_id    TEXT PRIMARY KEY,
    request_id   TEXT NOT NULL,
    exec_id      TEXT,
    pc_id        TEXT NOT NULL,
    -- NULL while in-flight (script spawned, not yet returned).
    exit_code    INTEGER,
    stdout       TEXT NOT NULL DEFAULT '',
    stderr       TEXT NOT NULL DEFAULT '',
    started_at   TIMESTAMP NOT NULL,
    -- NULL while in-flight. Once the matching ExecResult lands, the
    -- results projector UPDATEs this from NULL → real timestamp,
    -- transitioning the row from "running" to "finished" for any
    -- consumer querying via `finished_at IS NULL`.
    finished_at  TIMESTAMP,
    recorded_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    job_id       TEXT,
    -- New: pinned manifest version for the run. Populated by the
    -- events.started insert (whose payload carries Command.version);
    -- the results projector's ON CONFLICT DO UPDATE preserves it.
    version      TEXT,
    -- Set by the cleanup reaper when an in-flight row's ExecResult
    -- never arrives, so the projector's `finished_at IS NULL` UPSERT
    -- guard can tell a reaped placeholder from a genuinely-live row.
    reaped       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS executions (
    exec_id       TEXT PRIMARY KEY,
    job_id        TEXT NOT NULL,
    version       TEXT NOT NULL,
    initiated_by  TEXT NOT NULL,
    initiated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    target_count  INTEGER NOT NULL,
    success_count INTEGER NOT NULL DEFAULT 0,
    failure_count INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS host_perf_samples (
    pc_id                       TEXT NOT NULL,
    at                          TIMESTAMP NOT NULL,
    cpu_pct                     REAL,
    cpu_count                   INTEGER,
    mem_used_bytes              INTEGER,
    mem_total_bytes             INTEGER,
    swap_used_bytes             INTEGER,
    swap_total_bytes            INTEGER,
    disk_read_bytes_per_sec     REAL,
    disk_written_bytes_per_sec  REAL,
    net_rx_bytes_per_sec        REAL,
    net_tx_bytes_per_sec        REAL,
    PRIMARY KEY (pc_id, at)
);

CREATE TABLE IF NOT EXISTS inventory_facts (
    pc_id        TEXT NOT NULL,
    job_id       TEXT NOT NULL,
    facts_json   TEXT NOT NULL,
    display_json TEXT,
    summary_json TEXT,
    collected_at TIMESTAMP,
    recorded_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (pc_id, job_id)
);

CREATE TABLE IF NOT EXISTS inventory_history (
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

CREATE TABLE IF NOT EXISTS obs_events (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    pc_id             TEXT NOT NULL,
    at                TIMESTAMP NOT NULL,
    kind              TEXT NOT NULL,
    source            TEXT NOT NULL,
    event_record_id   TEXT,
    payload           TEXT,
    UNIQUE(pc_id, source, event_record_id)
);

CREATE TABLE IF NOT EXISTS process_perf_samples (
    pc_id                       TEXT NOT NULL,
    at                          TIMESTAMP NOT NULL,
    pid                         INTEGER NOT NULL,
    name                        TEXT NOT NULL,
    cpu_pct                     REAL NOT NULL,
    rss_bytes                   INTEGER NOT NULL,
    disk_read_bytes_per_sec     REAL,
    disk_written_bytes_per_sec  REAL,
    PRIMARY KEY (pc_id, at, pid)
);

CREATE TABLE IF NOT EXISTS users (
    username       TEXT PRIMARY KEY,
    password_hash  TEXT NOT NULL,                 -- argon2id PHC string
    role           TEXT NOT NULL CHECK(role IN ('viewer','operator','admin')),
    disabled       INTEGER NOT NULL DEFAULT 0,
    must_change_pw INTEGER NOT NULL DEFAULT 0,    -- forced reset on first login
    created_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_agents_heartbeat ON agents(last_heartbeat DESC);

CREATE INDEX IF NOT EXISTS idx_audit_action ON audit_log(action, occurred_at);

CREATE INDEX IF NOT EXISTS idx_audit_actor  ON audit_log(actor, occurred_at);

CREATE INDEX IF NOT EXISTS idx_execution_results_exec
    ON execution_results(exec_id, recorded_at DESC)
    WHERE exec_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_execution_results_inflight
    ON execution_results(started_at DESC)
    WHERE finished_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_execution_results_job
    ON execution_results(job_id, pc_id, finished_at DESC)
    WHERE job_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_execution_results_pc
    ON execution_results(pc_id, recorded_at DESC);

CREATE INDEX IF NOT EXISTS idx_execution_results_request
    ON execution_results(request_id);

CREATE INDEX IF NOT EXISTS idx_executions_job
    ON executions(job_id, initiated_at DESC);

CREATE INDEX IF NOT EXISTS idx_host_perf_samples_at ON host_perf_samples(at);

CREATE INDEX IF NOT EXISTS idx_inventory_facts_job ON inventory_facts(job_id);

CREATE INDEX IF NOT EXISTS idx_inventory_facts_pc  ON inventory_facts(pc_id);

CREATE INDEX IF NOT EXISTS idx_inventory_history_field
    ON inventory_history(job_id, field_path, observed_at DESC);

CREATE INDEX IF NOT EXISTS idx_inventory_history_identity
    ON inventory_history(job_id, field_path, identity_json);

CREATE INDEX IF NOT EXISTS idx_inventory_history_observed_at
    ON inventory_history(observed_at);

CREATE INDEX IF NOT EXISTS idx_inventory_history_pc
    ON inventory_history(pc_id, observed_at DESC);

CREATE INDEX IF NOT EXISTS idx_obs_events_at ON obs_events(at);

CREATE INDEX IF NOT EXISTS idx_obs_events_kind_at ON obs_events(kind, at DESC);

CREATE INDEX IF NOT EXISTS idx_obs_events_pc_at ON obs_events(pc_id, at DESC);

CREATE INDEX IF NOT EXISTS idx_process_perf_samples_at ON process_perf_samples(at);
