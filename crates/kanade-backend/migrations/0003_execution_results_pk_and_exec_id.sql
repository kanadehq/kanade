-- Issue #19 + α (see https://github.com/yukimemi/kanade/issues/19):
-- Rework `execution_results` so per-PC fan-out rows actually persist,
-- and add the missing back-link to the deployment that produced
-- each row. Two failures the old schema had:
--
--   * `request_id TEXT PRIMARY KEY` collided across PCs receiving the
--     same broadcast Command (commands.all / commands.group.X publish
--     one Command with one request_id; N agents each emit ExecResult
--     reusing that request_id). The projector's
--     `INSERT … ON CONFLICT(request_id) DO NOTHING` silently dropped
--     every result past the first PC. Multi-PC fan-out has been
--     losing data this entire time.
--
--   * No `exec_id` column meant the projector could not increment
--     `executions.success_count` / `failure_count` — the columns sat
--     unused since v0.16. Each ExecResult now carries `exec_id`
--     forwarded from `Command.exec_id` (newly renamed from job_id),
--     so the projector can attribute results to deployments.
--
-- SQLite can't ALTER the PK in place, so the standard table-swap
-- recipe: create the new shape, copy rows over (existing
-- `request_id` becomes the legacy `result_id` since back-then those
-- WERE unique within the surviving rows), drop the old, rename the
-- new. Old rows get `exec_id = NULL` — there's no way to recover the
-- mapping retroactively, and `NULL` correctly conveys "predates the
-- exec_id era". Indexes are recreated with the same shape.

CREATE TABLE execution_results_new (
    -- v0.29 / Issue #19: agent-minted UUID, unique per (Command, PC)
    -- run. Replaces request_id as the PK so multi-PC fan-out stops
    -- silently dropping results.
    result_id    TEXT PRIMARY KEY,
    -- Surviving identifier — the NATS reply token. NOT unique any
    -- more (broadcast Commands share it across PCs); useful for
    -- joining back to the `kanade run` request/reply path.
    request_id   TEXT NOT NULL,
    -- v0.29 / Issue #19: nullable back-link to `executions.exec_id`.
    -- NULL for rows that pre-date this migration AND for results
    -- from ad-hoc `kanade run` (no deployment behind them).
    exec_id      TEXT,
    pc_id        TEXT NOT NULL,
    exit_code    INTEGER NOT NULL,
    stdout       TEXT NOT NULL,
    stderr       TEXT NOT NULL,
    started_at   TIMESTAMP NOT NULL,
    finished_at  TIMESTAMP NOT NULL,
    recorded_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Carries `Manifest.id` (the cmd_id, not exec_id). Added in
    -- migration 0002; preserved here so the inventory projector keeps
    -- working without a separate column copy step.
    job_id       TEXT
);

-- Carry historical rows forward. Old `request_id` becomes
-- `result_id` (it was unique under the broken-but-deduped old PK so
-- this is safe). `exec_id` stays NULL — no provenance info
-- recoverable from old rows.
INSERT INTO execution_results_new (
    result_id, request_id, exec_id, pc_id, exit_code, stdout, stderr,
    started_at, finished_at, recorded_at, job_id
)
SELECT
    request_id, request_id, NULL, pc_id, exit_code, stdout, stderr,
    started_at, finished_at, recorded_at, job_id
FROM execution_results;

DROP TABLE execution_results;
ALTER TABLE execution_results_new RENAME TO execution_results;

-- Per-PC chronological scan, same shape as the migration 0001 index.
CREATE INDEX idx_execution_results_pc
    ON execution_results(pc_id, recorded_at DESC);

-- v0.19's per-job lookup index (was added in migration 0002).
-- Recreated post-swap so SQLite knows about it on the new table.
CREATE INDEX idx_execution_results_job
    ON execution_results(job_id, pc_id, finished_at DESC)
    WHERE job_id IS NOT NULL;

-- New: per-exec_id chronological scan for the upcoming
-- /api/executions detail view (Activity Running tab follow-up).
CREATE INDEX idx_execution_results_exec
    ON execution_results(exec_id, recorded_at DESC)
    WHERE exec_id IS NOT NULL;

-- Speed up the projector's "decode the request_id from a results.{X}
-- subject and look up its rows" path. Not unique any more (broadcast
-- shares request_id across PCs).
CREATE INDEX idx_execution_results_request
    ON execution_results(request_id);
