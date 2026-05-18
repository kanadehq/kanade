-- v0.16.0 — squashed baseline. Folds the v0.5..v0.14.1 migrations
-- into a single landing point. The user opted to wipe and recreate
-- rather than chain six small migrations forward.
--
-- Renames compared to pre-v0.16:
--   * deployment_results → execution_results
--   * deployments        → executions
--   * deploy_id (column) → exec_id (PK of executions)
--
-- agents holds the baseline heartbeat-derived row only; everything
-- richer (CPU model, RAM, disks, OS detail) lives in
-- inventory_facts under operator-defined probes (see v0.13/0.14
-- inventory generalisation).

CREATE TABLE IF NOT EXISTS agents (
    pc_id           TEXT PRIMARY KEY,
    hostname        TEXT,
    last_heartbeat  TIMESTAMP,
    agent_version   TEXT,
    os_family       TEXT,
    updated_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_agents_heartbeat ON agents(last_heartbeat DESC);

-- One row per ExecResult flowing through RESULTS. The projector
-- inserts ON CONFLICT DO NOTHING so redeliveries stay idempotent.
CREATE TABLE IF NOT EXISTS execution_results (
    request_id   TEXT PRIMARY KEY,
    pc_id        TEXT NOT NULL,
    exit_code    INTEGER NOT NULL,
    stdout       TEXT NOT NULL,
    stderr       TEXT NOT NULL,
    started_at   TIMESTAMP NOT NULL,
    finished_at  TIMESTAMP NOT NULL,
    recorded_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_execution_results_pc
    ON execution_results(pc_id, recorded_at DESC);

-- One row per `kanade exec` invocation (or scheduler fire).
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

CREATE INDEX IF NOT EXISTS idx_executions_job
    ON executions(job_id, initiated_at DESC);

-- Operator-defined inventory probes (PowerShell scripts emitting
-- JSON) project here keyed by (pc_id, job_id). display_json +
-- summary_json snapshot the manifest's render hints at projection
-- time, so editing the job's display config later doesn't rewrite
-- how old facts render.
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

CREATE INDEX IF NOT EXISTS idx_inventory_facts_pc  ON inventory_facts(pc_id);
CREATE INDEX IF NOT EXISTS idx_inventory_facts_job ON inventory_facts(job_id);

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
