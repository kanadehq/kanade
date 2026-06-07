-- #399: the Activity listing (/api/results) now filters (`since`) and
-- sorts on started_at instead of recorded_at. The only existing index
-- on started_at is the partial in-flight one (scoped to
-- finished_at IS NULL), which can't serve the full-history
-- ORDER BY started_at DESC LIMIT ?, so add a full index.
-- result_id rides along as the ORDER BY tie-breaker so equal-instant
-- rows (broadcast fan-outs) come back in a deterministic order
-- straight off the index.
CREATE INDEX IF NOT EXISTS idx_execution_results_started_at
    ON execution_results (started_at DESC, result_id DESC);
