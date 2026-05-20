-- v0.30 / PR α' unified lifecycle: stop maintaining a separate
-- `running_runs` table for in-flight rows. `execution_results` now
-- carries both states — a row is created when `events.started` lands
-- (with `finished_at = NULL`) and the same row is UPDATEd when the
-- matching `ExecResult` arrives. The SPA Activity table consumes a
-- single source of truth; Running vs Finished becomes a status
-- filter on the existing screen, not a separate tab.
--
-- Schema changes (SQLite ALTER COLUMN limitations → table swap):
--   * `finished_at` becomes NULLABLE (was NOT NULL since v0.16).
--   * `exit_code` becomes NULLABLE (was NOT NULL); NULL means "still
--     running, no result yet".
--   * new `version TEXT` column — forwarded from `Command.version`
--     via `events.started` so the Running view can show what version
--     of a script is in flight without a JOIN through executions.
--
-- Indexes carried forward unchanged; one new partial index for the
-- Activity Running filter's hot path.

CREATE TABLE execution_results_new (
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
    version      TEXT
);

INSERT INTO execution_results_new (
    result_id, request_id, exec_id, pc_id, exit_code, stdout, stderr,
    started_at, finished_at, recorded_at, job_id, version
)
SELECT
    result_id, request_id, exec_id, pc_id, exit_code, stdout, stderr,
    started_at, finished_at, recorded_at, job_id, NULL
FROM execution_results;

DROP TABLE execution_results;
ALTER TABLE execution_results_new RENAME TO execution_results;

-- Existing indexes from migrations 0001 / 0002 / 0003.
CREATE INDEX idx_execution_results_pc
    ON execution_results(pc_id, recorded_at DESC);
CREATE INDEX idx_execution_results_job
    ON execution_results(job_id, pc_id, finished_at DESC)
    WHERE job_id IS NOT NULL;
CREATE INDEX idx_execution_results_exec
    ON execution_results(exec_id, recorded_at DESC)
    WHERE exec_id IS NOT NULL;
CREATE INDEX idx_execution_results_request
    ON execution_results(request_id);

-- New: Activity Running tab hot path.
-- `SELECT ... WHERE finished_at IS NULL ORDER BY started_at DESC`.
-- Partial index keeps it small (only in-flight rows) and free of
-- the bulk of historical recorded_at fanout.
CREATE INDEX idx_execution_results_inflight
    ON execution_results(started_at DESC)
    WHERE finished_at IS NULL;
