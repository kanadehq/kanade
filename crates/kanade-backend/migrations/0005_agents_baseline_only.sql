-- v0.14.0 — the hardcoded WMI inventory loop is gone. agents table
-- now stores ONLY the baseline heartbeat-derived fields; everything
-- richer (CPU model, RAM, disks, OS detail) lives in
-- inventory_facts under operator-defined probes.
--
-- Older rows had columns set from the v0.12/v0.13 hardcoded
-- inventory projector. Drop those columns so the schema reflects
-- the new world; existing rows preserve their pc_id + heartbeat
-- fields.

ALTER TABLE agents DROP COLUMN os_name;
ALTER TABLE agents DROP COLUMN os_version;
ALTER TABLE agents DROP COLUMN os_build;
ALTER TABLE agents DROP COLUMN cpu_model;
ALTER TABLE agents DROP COLUMN cpu_cores;
ALTER TABLE agents DROP COLUMN ram_bytes;
ALTER TABLE agents DROP COLUMN disks_json;
ALTER TABLE agents DROP COLUMN last_inventory;
