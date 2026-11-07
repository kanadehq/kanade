-- #1666: `/api/results` pushes exact literal regex filters on `pc_id`,
-- `job_id`, and `exec_id` (`^<literal>$` without flags or regex
-- operators) down to SQL equality while ordering by
-- `(started_at DESC, result_id DESC)` and paging via
-- `(started_at, result_id) < (?, ?)`.
--
-- The baseline indexes (`idx_execution_results_pc`, `_job`, `_exec`)
-- sort by `recorded_at DESC` or `finished_at DESC`, forcing SQLite into
-- `USE TEMP B-TREE FOR ORDER BY` whenever an identifier filter is paired
-- with `ORDER BY started_at DESC, result_id DESC`. Indexing each
-- identifier with `(started_at DESC, result_id DESC)` serves both the
-- initial range scan and cursor continuation straight from the B-tree.

CREATE INDEX IF NOT EXISTS idx_execution_results_pc_started_at
    ON execution_results (pc_id, started_at DESC, result_id DESC);

CREATE INDEX IF NOT EXISTS idx_execution_results_job_started_at
    ON execution_results (job_id, started_at DESC, result_id DESC)
    WHERE job_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_execution_results_exec_started_at
    ON execution_results (exec_id, started_at DESC, result_id DESC)
    WHERE exec_id IS NOT NULL;
