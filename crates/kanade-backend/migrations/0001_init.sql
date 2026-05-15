-- Spec §2.3.4. Minimal projection schema for Sprint 3a.
-- agents stores the latest HW inventory snapshot per PC.
-- deployment_results stores every ExecResult that flowed through RESULTS.

CREATE TABLE IF NOT EXISTS agents (
    pc_id            TEXT PRIMARY KEY,
    hostname         TEXT,
    os_name          TEXT,
    os_version       TEXT,
    os_build         TEXT,
    cpu_model        TEXT,
    cpu_cores        INTEGER,
    ram_bytes        BIGINT,
    disks_json       TEXT,
    last_inventory   TIMESTAMP,
    updated_at       TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS deployment_results (
    request_id   TEXT PRIMARY KEY,
    pc_id        TEXT NOT NULL,
    exit_code    INTEGER NOT NULL,
    stdout       TEXT NOT NULL,
    stderr       TEXT NOT NULL,
    started_at   TIMESTAMP NOT NULL,
    finished_at  TIMESTAMP NOT NULL,
    recorded_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_results_pc ON deployment_results(pc_id, recorded_at DESC);

-- audit_log is wired in Sprint 3c, but the schema lands now so sqlx::migrate!
-- doesn't churn on the next sprint.
CREATE TABLE IF NOT EXISTS audit_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    actor        TEXT NOT NULL,
    action       TEXT NOT NULL,
    target       TEXT,
    payload      TEXT,
    occurred_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_audit_actor  ON audit_log(actor, occurred_at);
CREATE INDEX IF NOT EXISTS idx_audit_action ON audit_log(action, occurred_at);
