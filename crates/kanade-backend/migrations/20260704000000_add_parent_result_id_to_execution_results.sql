-- #955: link a `finalize:` hook's own result row back to the run that
-- triggered it. The agent stamps the finalize ExecResult's
-- `parent_result_id` with the parent run's `result_id`; the SPA renders
-- the two directions (run -> its finalize, finalize -> its run).
-- NULL for every ordinary run (only finalize rows set it).
ALTER TABLE execution_results ADD COLUMN parent_result_id TEXT;

-- Reverse lookup: `... WHERE parent_result_id = ?` on the detail
-- endpoint finds a run's finalize child rows. Partial index keeps it
-- tiny — only finalize rows have a non-NULL value.
CREATE INDEX IF NOT EXISTS idx_execution_results_parent_result_id
    ON execution_results (parent_result_id)
    WHERE parent_result_id IS NOT NULL;
