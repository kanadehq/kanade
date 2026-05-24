-- v0.40 Part 1: host-wide perf telemetry. Time-series table that
-- the host_perf projector appends to on every `host_perf.<pc_id>`
-- publish (default 60 s cadence). Distinct from the agent-self-perf
-- columns on the `agents` table — those are a live "latest value"
-- mirror; this one is the historical record that powers the SPA's
-- per-PC charts and (in Phase 3) the fleet-wide dashboard cards.
--
-- Storage shape:
--   * pc_id / at — composite PK so per-PC range scans (the SPA's
--                  primary access pattern) hit the table's natural
--                  clustered order without a secondary index.
--   * All metric columns nullable: forward-compat with agents that
--                  fail to populate a given counter (sandbox without
--                  swap, first tick after restart where the disk /
--                  net rate has no prior sample to diff against).
--   * Rates are stored as f64 in bytes/sec — the agent computes the
--                  diff between successive cumulative sysinfo samples
--                  and divides by elapsed wall time, so the backend
--                  never needs to know the previous row.
--
-- Retention: 30 days. The cleanup loop (crates/kanade-backend/src/
-- cleanup.rs) prunes rows older than that on its 5 min cadence. At
-- 60 s cadence × 30 d × 1000 PCs = ~43 M rows, which SQLite handles
-- comfortably on a single backend. If fleets grow past that we'll add
-- a rollup pass (24h raw / 7d 5min / 30d 1h) in a follow-up — the
-- table shape doesn't need to change for that.

CREATE TABLE host_perf_samples (
    pc_id                       TEXT NOT NULL,
    at                          TIMESTAMP NOT NULL,
    cpu_pct                     REAL,
    cpu_count                   INTEGER,
    mem_used_bytes              INTEGER,
    mem_total_bytes             INTEGER,
    swap_used_bytes             INTEGER,
    swap_total_bytes            INTEGER,
    disk_read_bytes_per_sec     REAL,
    disk_written_bytes_per_sec  REAL,
    net_rx_bytes_per_sec        REAL,
    net_tx_bytes_per_sec        REAL,
    PRIMARY KEY (pc_id, at)
);

-- Cross-PC range scans (fleet-wide aggregates in Phase 3) need to
-- walk by `at` first; the composite PK alone can't serve that
-- direction efficiently.
CREATE INDEX idx_host_perf_samples_at ON host_perf_samples(at);
