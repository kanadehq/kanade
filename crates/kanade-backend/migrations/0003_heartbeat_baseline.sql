-- v0.12.0 — Heartbeat is no longer just a liveness ping the ping API
-- subscribes to live: the heartbeat projector now upserts a baseline
-- agents row from every incoming Heartbeat, so the UI shows an agent
-- within 30 s of boot even when the WMI-backed inventory hasn't run
-- (or can't run) yet.
--
-- New columns are nullable so the existing inventory projector can
-- keep filling its richer set without colliding. `last_heartbeat` is
-- separate from `last_inventory` so the SPA Dashboard can distinguish
-- "alive but inventory stale" from "fully fresh".

ALTER TABLE agents ADD COLUMN last_heartbeat  TIMESTAMP;
ALTER TABLE agents ADD COLUMN agent_version   TEXT;
ALTER TABLE agents ADD COLUMN os_family       TEXT;

CREATE INDEX IF NOT EXISTS idx_agents_heartbeat ON agents(last_heartbeat DESC);
