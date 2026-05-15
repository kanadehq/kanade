-- Spec §2.3.4. One row per `kanade deploy` invocation; success_count /
-- failure_count get rolled forward by the audit / results worker in 3c.

CREATE TABLE IF NOT EXISTS deployments (
    deploy_id     TEXT PRIMARY KEY,
    job_id        TEXT NOT NULL,
    version       TEXT NOT NULL,
    initiated_by  TEXT NOT NULL,
    initiated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    target_count  INTEGER NOT NULL,
    success_count INTEGER NOT NULL DEFAULT 0,
    failure_count INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'pending'
);

CREATE INDEX IF NOT EXISTS idx_deploy_job ON deployments(job_id, initiated_at DESC);
