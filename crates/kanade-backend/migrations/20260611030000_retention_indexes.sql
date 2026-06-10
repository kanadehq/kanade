-- #486: retention sweeps for the three previously-unbounded tables
-- (execution_results / executions / audit_log) filter on these
-- columns every 5-minute cleanup tick. Without an index the no-op
-- common case ("nothing old enough") is a full table scan of the
-- hottest tables; with it, an index probe.
--
-- idx_audit_log_occurred_at additionally serves the /api/audit
-- newest-first listing, which previously sorted the whole table
-- when no actor/action filter was given (#516).
CREATE INDEX IF NOT EXISTS idx_execution_results_recorded_at
    ON execution_results (recorded_at);
CREATE INDEX IF NOT EXISTS idx_executions_initiated_at
    ON executions (initiated_at);
CREATE INDEX IF NOT EXISTS idx_audit_log_occurred_at
    ON audit_log (occurred_at);
