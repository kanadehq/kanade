-- PR #330 follow-up: distinguish a backend-reaped placeholder row
-- from a genuinely-finished one.
--
-- The cleanup task (`reap_orphaned_results`) stamps `finished_at` on
-- in-flight rows whose `ExecResult` never arrived (agent died /
-- pre-#330 kill-hang), so they stop showing "実行中" forever. But the
-- results projector's UPSERT guards its DO UPDATE with
-- `WHERE execution_results.finished_at IS NULL`: once a row is reaped,
-- a *real* `ExecResult` that arrives later (network partition heals,
-- backend outage ends, or a job legitimately runs past the 24 h
-- threshold) would be silently dropped, freezing the row on the
-- reaped placeholder (gemini review, PR #332).
--
-- This `reaped` flag lets the projector recognise a placeholder and
-- overwrite it with the real result (the projector's DO UPDATE WHERE
-- becomes `finished_at IS NULL OR reaped = 1`, and clears the flag on
-- update). 0 = normal (in-flight or genuinely finished); 1 = reaped
-- placeholder, safe to overwrite.
--
-- Plain ADD COLUMN with a constant DEFAULT — no table rebuild, and
-- every existing row reads back 0 (none were reaped before this).

ALTER TABLE execution_results
    ADD COLUMN reaped INTEGER NOT NULL DEFAULT 0;
