-- v0.30 / PR α: per-(exec_id, pc_id) "this PC is running this
-- deployment right now" projection, populated from the new
-- `events.started.<exec_id>.<pc_id>` lifecycle event the agent
-- emits before script spawn. Drives the SPA Activity Running tab
-- and any future per-PC kill / pause UI.
--
-- Lifecycle:
--   1. agent publishes events.started → events projector inserts
--      a row with `finished_at = NULL`
--   2. agent eventually publishes ExecResult → results projector
--      UPDATEs the matching row's `finished_at`, NOT deletes it,
--      so a JetStream redelivery of the SAME started event after
--      the result lands can ON CONFLICT-skip cleanly.
--   3. SPA Running tab queries `WHERE finished_at IS NULL`
--   4. periodic cleanup purges rows where `finished_at` is older
--      than 7d (matching STREAM_EVENTS retention)
--
-- Two race-safety mechanisms cooperate:
--   * `ON CONFLICT(exec_id, pc_id) DO NOTHING` on the events.started
--     insert dedupes JetStream redeliveries (same-direction race).
--   * `WHERE NOT EXISTS (SELECT 1 FROM execution_results WHERE
--     exec_id = ? AND pc_id = ?)` on the same insert prevents the
--     out-of-order ghost: if a started event redelivery arrives
--     AFTER its matching ExecResult, the NOT EXISTS check sees the
--     finished-already row in execution_results and silently no-
--     ops. Without this guard the redelivered start would create a
--     fresh row with `finished_at = NULL` — a "running" ghost in
--     the Activity Running tab for a long-since-finished run.

CREATE TABLE running_runs (
    exec_id      TEXT NOT NULL,
    pc_id        TEXT NOT NULL,
    started_at   TIMESTAMP NOT NULL,
    manifest_id  TEXT NOT NULL,
    version      TEXT NOT NULL,
    recorded_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- NULL while the script is still running. UPDATEd by the
    -- results projector when the matching ExecResult lands. Lets
    -- the Running view filter cleanly without DELETE-on-arrival
    -- race conditions (which would let an out-of-order start
    -- event re-insert a "running" row for an already-finished
    -- script).
    finished_at  TIMESTAMP,
    PRIMARY KEY (exec_id, pc_id)
);

-- Hot path index: SPA Running tab does
-- `SELECT ... WHERE finished_at IS NULL ORDER BY started_at DESC`.
-- Partial index keeps it small (only in-flight rows) and
-- chronological ordering free.
CREATE INDEX idx_running_runs_inflight
    ON running_runs(started_at DESC)
    WHERE finished_at IS NULL;

-- Per-exec scan for the Activity detail view ("all PCs of this
-- deployment, running or finished").
CREATE INDEX idx_running_runs_exec
    ON running_runs(exec_id, started_at DESC);
