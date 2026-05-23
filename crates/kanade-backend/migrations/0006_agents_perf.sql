-- v0.37 Part 2: agent process self-perf columns surfaced by the
-- enriched heartbeat (#130). All nullable so older agents (or any
-- future build where sysinfo fails) keep upserting cleanly via
-- the existing heartbeat projector.
--
-- Storage shape:
--   * agent_cpu_pct           — float, percent-of-one-core (1 core
--                               at 100 %, 2 cores fully busy = 200)
--   * agent_rss_bytes         — bytes (signed because sqlx::query!
--                               binds i64; clamped on the agent
--                               side before send)
--   * agent_disk_read_bytes   — absolute cumulative bytes since
--                               process start. We store the raw
--                               cumulative number; if a future
--                               consumer wants a rate it diffs
--                               successive snapshots itself.
--   * agent_disk_written_bytes — same shape as read
--
-- No new index — the agents table is already small (one row per
-- PC in the fleet) and the SPA reads everything via a plain SELECT.

ALTER TABLE agents ADD COLUMN agent_cpu_pct           REAL;
ALTER TABLE agents ADD COLUMN agent_rss_bytes         INTEGER;
ALTER TABLE agents ADD COLUMN agent_disk_read_bytes   INTEGER;
ALTER TABLE agents ADD COLUMN agent_disk_written_bytes INTEGER;
