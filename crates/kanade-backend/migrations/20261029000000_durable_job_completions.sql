-- Permanent scheduler state (#1660), independent of execution history retention.
-- No FK to execution_results, executions, agents or manifests: deletion/replay
-- must not re-arm a successful job. Unknown versions count only version-blind.
CREATE TABLE job_completions (
    job_id TEXT NOT NULL,
    pc_id TEXT NOT NULL,
    last_success_at TIMESTAMP NOT NULL,
    PRIMARY KEY (job_id, pc_id)
);
CREATE TABLE job_version_completions (
    job_id TEXT NOT NULL,
    pc_id TEXT NOT NULL,
    version TEXT NOT NULL,
    last_success_at TIMESTAMP NOT NULL,
    PRIMARY KEY (job_id, version, pc_id)
);

-- Backfill only retained, finished exit-0 results (manual exec included).
-- Already-pruned history cannot be reconstructed; never infer a missing version.
INSERT INTO job_completions
SELECT job_id, pc_id, MAX(finished_at) FROM execution_results
WHERE job_id IS NOT NULL AND exit_code = 0 AND finished_at IS NOT NULL
GROUP BY job_id, pc_id;
INSERT INTO job_version_completions
SELECT job_id, pc_id, version, MAX(finished_at) FROM execution_results
WHERE job_id IS NOT NULL AND version IS NOT NULL
  AND exit_code = 0 AND finished_at IS NOT NULL
GROUP BY job_id, pc_id, version;

-- Capture successful inserts atomically with result persistence. Replays
-- cannot move the last success backwards.
CREATE TRIGGER execution_results_completion_insert
AFTER INSERT ON execution_results
WHEN NEW.job_id IS NOT NULL AND NEW.exit_code = 0 AND NEW.finished_at IS NOT NULL
BEGIN
    INSERT INTO job_completions (job_id, pc_id, last_success_at)
    VALUES (NEW.job_id, NEW.pc_id, NEW.finished_at)
    ON CONFLICT (job_id, pc_id) DO UPDATE SET
        last_success_at = MAX(last_success_at, excluded.last_success_at);
    INSERT INTO job_version_completions (job_id, pc_id, version, last_success_at)
    SELECT NEW.job_id, NEW.pc_id, NEW.version, NEW.finished_at
    WHERE NEW.version IS NOT NULL
    ON CONFLICT (job_id, version, pc_id) DO UPDATE SET
        last_success_at = MAX(last_success_at, excluded.last_success_at);
END;

-- UPDATE also captures started -> success, late results after reaping, and
-- metadata enrichment. Replays cannot move the last success backwards.
CREATE TRIGGER execution_results_completion_update
AFTER UPDATE OF exit_code, finished_at, job_id, pc_id, version ON execution_results
WHEN NEW.job_id IS NOT NULL AND NEW.exit_code = 0 AND NEW.finished_at IS NOT NULL
BEGIN
    INSERT INTO job_completions (job_id, pc_id, last_success_at)
    VALUES (NEW.job_id, NEW.pc_id, NEW.finished_at)
    ON CONFLICT (job_id, pc_id) DO UPDATE SET
        last_success_at = MAX(last_success_at, excluded.last_success_at);
    INSERT INTO job_version_completions (job_id, pc_id, version, last_success_at)
    SELECT NEW.job_id, NEW.pc_id, NEW.version, NEW.finished_at
    WHERE NEW.version IS NOT NULL
    ON CONFLICT (job_id, version, pc_id) DO UPDATE SET
        last_success_at = MAX(last_success_at, excluded.last_success_at);
END;
