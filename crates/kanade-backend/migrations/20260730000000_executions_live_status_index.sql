-- #1214③: the /api/jobs LIVE column runs
--   WHERE status IN ('running','pending') GROUP BY job_id
-- over `executions`, a table that grows forever. The pre-existing
-- indexes cover (job_id, initiated_at) and (initiated_at), so every
-- Jobs-page load full-scanned the table to find the handful of
-- in-flight rows. A partial index holds only in-flight rows, stays
-- tiny, and turns the scan into a seek.
CREATE INDEX IF NOT EXISTS idx_executions_live
    ON executions (status, job_id)
    WHERE status IN ('running', 'pending');
