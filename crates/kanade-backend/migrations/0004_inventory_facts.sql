-- v0.13.0 — Operator-defined inventory probes (PowerShell scripts
-- emitting JSON, scheduled like any other job) project into this
-- table keyed by (pc_id, job_id). The results projector spots
-- ExecResult.job_id, looks up the schedule's manifest, and if the
-- manifest carries an `inventory:` hint upserts the row here.
--
-- Phase 1: lives alongside the existing typed columns on `agents`
-- (populated by the v0.12 hardcoded WMI inventory). Phase 2 will
-- drop the typed columns once a default `configs/jobs/inventory-
-- hw.yaml` is the only path filling them.

CREATE TABLE IF NOT EXISTS inventory_facts (
    pc_id        TEXT NOT NULL,
    job_id       TEXT NOT NULL,
    facts_json   TEXT NOT NULL,
    -- The display hint as the manifest declared it at projection
    -- time. Stored alongside the facts so the SPA can render with
    -- the right columns + types without separately querying the
    -- schedule bucket.
    display_json TEXT,
    collected_at TIMESTAMP,
    recorded_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (pc_id, job_id)
);

CREATE INDEX IF NOT EXISTS idx_inventory_facts_pc  ON inventory_facts(pc_id);
CREATE INDEX IF NOT EXISTS idx_inventory_facts_job ON inventory_facts(job_id);
