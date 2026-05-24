-- v0.41 / Phase 2: per-process perf time-series. Opt-in per-PC —
-- only populated while `process_perf_enabled=true` AND the deadline
-- (`process_perf_expires_at`) is still in the future, so the table
-- only grows during an operator's active investigation window. When
-- the deadline passes the agent stops publishing without any backend
-- action; the existing cleanup loop prunes rows older than the
-- retention window.
--
-- Storage shape:
--   * pc_id / at / pid — composite PK so per-PC range scans + per-PID
--                  selections both hit the natural clustered order.
--                  We include `pid` in the PK because a single tick
--                  carries N rows (top-N by CPU), one per process.
--   * cpu_pct / rss_bytes — NOT NULL: the agent always emits these
--                  when it emits at all (sysinfo guarantees them).
--   * disk_*_per_sec — nullable: the agent reports None on the first
--                  sample for a PID (no prior baseline to diff).
--
-- Retention: 7 days. Process-perf data is much higher cardinality
-- than host-perf (N rows per tick instead of 1), and the operator
-- use case is "investigation now / a few hours back", not
-- "monthly trend". A shorter retention keeps storage well-bounded
-- even when several PCs are simultaneously in investigation mode.

CREATE TABLE process_perf_samples (
    pc_id                       TEXT NOT NULL,
    at                          TIMESTAMP NOT NULL,
    pid                         INTEGER NOT NULL,
    name                        TEXT NOT NULL,
    cpu_pct                     REAL NOT NULL,
    rss_bytes                   INTEGER NOT NULL,
    disk_read_bytes_per_sec     REAL,
    disk_written_bytes_per_sec  REAL,
    PRIMARY KEY (pc_id, at, pid)
);

-- The PK already orders by (pc_id, at, pid) so a per-PC time-range
-- scan walks the table efficiently. The dedicated `at`-only index
-- supports the retention sweep (`DELETE WHERE at < ...`) and any
-- future fleet-wide aggregation.
CREATE INDEX idx_process_perf_samples_at ON process_perf_samples(at);
