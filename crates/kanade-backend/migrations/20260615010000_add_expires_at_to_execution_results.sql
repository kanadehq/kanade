-- #682: per-run reap deadline for in-flight rows.
--
-- The backend stamps `expires_at = recorded_at + timeout + slack` when
-- it projects `events.started` (see projector/events.rs::reap_deadline),
-- and the cleanup reaper gives up on an in-flight row once
-- `now > expires_at` instead of a flat 24h. This reaps an abandoned
-- placeholder (agent died mid-run) within roughly the run's own timeout
-- rather than a day later, while a legitimately long run keeps its full
-- timeout. `recorded_at` is the backend/NATS-side publish clock (not the
-- agent's `started_at`), so the deadline is immune to agent clock skew
-- vs the reaper's `now`.
--
-- NULL for legacy rows and any run whose manifest/timeout the projector
-- could not resolve; the reaper still handles those via the flat
-- INFLIGHT_TIMEOUT_HOURS fallback on `started_at`.
ALTER TABLE execution_results ADD COLUMN expires_at TIMESTAMP;

-- Partial index scoped to in-flight rows so the reaper's
-- `expires_at < ?` sweep stays a small range scan, mirroring the
-- existing idx_execution_results_inflight on started_at.
CREATE INDEX IF NOT EXISTS idx_execution_results_expires
    ON execution_results(expires_at)
    WHERE finished_at IS NULL;
