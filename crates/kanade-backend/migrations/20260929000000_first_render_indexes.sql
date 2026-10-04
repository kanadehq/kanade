-- Measurements behind these indexes (sqlite3 CLI, file DB, 400k obs_events rows
-- over 500 PCs, 100k inventory_facts rows = 5000 PCs x 20 jobs, ~2 KB facts_json):
--
--   default path, already fast, left unchanged:
--     Events list (shipped LIST_ANY_PC, since=2d, LIMIT 200)
--                                    SCAN idx_obs_events_at, no temp B-tree   <10 ms
--     DISTINCT kind                  idx_obs_events_kind_at                   <10 ms
--     by-job COUNT(*) WHERE job_id   COVERING idx_inventory_facts_job         ~10 ms
--     by-job ORDER BY pc_id LIMIT 50 COVERING sqlite_autoindex (pc_id,job_id) <10 ms
--   slow, fixed below:
--     DISTINCT source                SCAN obs_events (table)   80 ms -> <10 ms
--     COUNT(DISTINCT pc_id) BY job   SCAN via idx_..._job     160 ms ->  10 ms
--   (SQL only; HTTP/NATS latency was not measured.)

-- First-render indexes for the Events and Inventory pages.
--
-- Note: CREATE INDEX takes a write lock for the duration of the build, so on a
-- very large obs_events table the first start after upgrade is briefly slower.

-- `SELECT DISTINCT source FROM obs_events` (the Events source picker) had no
-- index and walked the whole table; a covering index turns it into an index scan.
CREATE INDEX IF NOT EXISTS idx_obs_events_source ON obs_events(source);

-- `SELECT job_id, COUNT(DISTINCT pc_id) ... GROUP BY job_id` (Inventory jobs
-- sidebar) read every facts row, including the multi-KB facts_json. Covering
-- (job_id, pc_id) answers it from the index alone.
CREATE INDEX IF NOT EXISTS idx_inventory_facts_job_pc ON inventory_facts(job_id, pc_id);
