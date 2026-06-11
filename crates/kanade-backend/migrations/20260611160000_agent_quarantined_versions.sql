-- #582 Phase 2: per-agent boot-sentinel quarantine list. The agent
-- reports, in every heartbeat, the versions its boot sentinel rolled
-- back after they crash-looped on boot. Stored as a JSON array of
-- version strings (NULL / absent = none) so the SPA rollout view can
-- flag which PCs failed to adopt a target version.
ALTER TABLE agents ADD COLUMN quarantined_versions TEXT;
