-- v0.14.1 — inventory probes can declare a separate `summary:`
-- column set for the fleet-wide "row per PC" view (`/inventory`
-- with no pc selected). Snapshot it alongside the detail
-- `display_json` so the SPA's fleet-list render doesn't have to
-- reach back into schedules KV for every page load, and so a
-- later manifest edit doesn't retroactively change how old facts
-- are rendered.

ALTER TABLE inventory_facts ADD COLUMN summary_json TEXT;
