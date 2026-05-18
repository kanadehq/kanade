-- v0.19.0 — `execution_results.job_id` lets the scheduler's dedup
-- policy (mode = once_per_pc / once_per_target) ask
-- "which pcs have already succeeded at this manifest?"
-- without joining back through `executions`.
--
-- Existing rows from before v0.19 get NULL (we don't backfill from
-- the executions table because the join is per-Command request_id,
-- and the projector wasn't recording the link). Schedules with
-- dedup modes only consider rows with job_id IS NOT NULL, so legacy
-- rows are silently ignored — at worst the first post-upgrade tick
-- fires at every pc again, which is the safe direction.

ALTER TABLE execution_results ADD COLUMN job_id TEXT;

CREATE INDEX IF NOT EXISTS idx_execution_results_job_pc
    ON execution_results(job_id, pc_id, finished_at DESC)
    WHERE job_id IS NOT NULL;
