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
