-- #516: /api/health/scan_durations filters on `finished_at >= ?`.
-- The only index touching finished_at is the composite
-- (job_id, pc_id, finished_at DESC), whose leading columns make it
-- useless for a bare finished_at range (leading-column rule), so the
-- query full-scanned execution_results — a table that grows with
-- fleet size × job count until the #486 retention sweep trims it.
--
-- Partial (finished rows only): the query always pairs the range
-- with `finished_at IS NOT NULL`, which matches the index predicate,
-- and in-flight rows then never pay the index-maintenance cost —
-- same pattern as the baseline idx_execution_results_inflight.
CREATE INDEX IF NOT EXISTS idx_execution_results_finished_at
    ON execution_results (finished_at)
    WHERE finished_at IS NOT NULL;
