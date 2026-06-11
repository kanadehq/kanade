// Wire types mirroring the Rust side. Hand-maintained for now;
// when the surface grows past ~10 types we should switch to
// generating from a shared OpenAPI / TypeScript export.

// v0.14: baseline only. Richer facts live in inventory_facts —
// see /api/inventory/<pc_id> + the Inventory SPA page.
export type AgentRow = {
  pc_id: string;
  hostname: string | null;
  os_family: string | null;
  agent_version: string | null;
  last_heartbeat: string | null;
  updated_at: string | null;
  // v0.37 Part 2: agent process self-perf — populated by 0.37+
  // agents. Older agents leave these null and the SPA renders a
  // blank cell. `agent_cpu_pct` is percent-of-one-core (sysinfo
  // convention; a process pegging 2 cores = 200). `*_bytes` are
  // absolute since process start; the SPA can diff successive
  // snapshots if it wants a rate.
  agent_cpu_pct: number | null;
  agent_rss_bytes: number | null;
  agent_disk_read_bytes: number | null;
  agent_disk_written_bytes: number | null;
  // #582 Phase 2: versions this agent's boot sentinel rolled back
  // after a crash-loop on boot (and now refuses to re-deploy). Drives
  // the Rollout page's "failed to adopt target" view. Empty / omitted
  // for agents that never reported any (pre-#582). Defaulted in case
  // an older backend response omits the key.
  quarantined_versions?: string[];
};

export type Heartbeat = {
  pc_id: string;
  at: string;
  agent_version: string;
  hostname: string | null;
  os_family: string | null;
};

export type ExecResult = {
  request_id: string;
  pc_id: string;
  exit_code: number;
  stdout: string;
  stderr: string;
  started_at: string;
  finished_at: string;
};

export type AgentGroups = {
  groups: string[];
};

export type ConfigScope = {
  target_version?: string;
  target_version_jitter?: string;
  heartbeat_interval?: string;
  host_perf_interval?: string;
  // v0.41 / Phase 2: opt-in per-process telemetry. See
  // [`ConfigScope::process_perf_enabled`] on the Rust side.
  process_perf_enabled?: boolean;
  process_perf_expires_at?: string;
  process_perf_top_n?: number;
};

export type EffectiveConfig = {
  target_version: string | null;
  target_version_jitter: string;
  heartbeat_interval: string;
  host_perf_interval: string;
  process_perf_enabled: boolean;
  process_perf_expires_at: string | null;
  process_perf_top_n: number;
};

export type EffectiveConfigResponse = {
  pc_id: string;
  effective: EffectiveConfig;
  warnings: string[];
};

export type JetstreamSnapshot = {
  streams: { name: string; exists: boolean }[];
  kv_buckets: { name: string; exists: boolean }[];
  object_stores: { name: string; exists: boolean }[];
};

// v0.40 Part 1: per-PC host-wide perf time-series response from
// /api/agents/{pc_id}/perf. Backend buckets in SQL via the `step`
// query param (default 5 min) so this array is bounded regardless
// of zoom level.
export type PerfPoint = {
  at: string;
  cpu_pct: number | null;
  mem_used_bytes: number | null;
  mem_total_bytes: number | null;
  swap_used_bytes: number | null;
  swap_total_bytes: number | null;
  disk_read_bytes_per_sec: number | null;
  disk_written_bytes_per_sec: number | null;
  net_rx_bytes_per_sec: number | null;
  net_tx_bytes_per_sec: number | null;
};

export type PerfResponse = {
  pc_id: string;
  from: string;
  to: string;
  step_seconds: number;
  points: PerfPoint[];
};

// v0.41 / Phase 2: per-PC top-N per-process snapshot from
// /api/agents/{pc_id}/processes. Empty `processes` + null `latest_at`
// when process_perf has never been enabled for this PC.
export type ProcessRow = {
  pid: number;
  name: string;
  cpu_pct: number;
  rss_bytes: number;
  disk_read_bytes_per_sec: number | null;
  disk_written_bytes_per_sec: number | null;
};

export type ProcessesResponse = {
  pc_id: string;
  latest_at: string | null;
  processes: ProcessRow[];
};

// v0.42: per-process bucketed time-series from
// /api/agents/{pc_id}/processes/timeline. Drives the stacked-area
// chart in AgentProcessSection. `names` is the stable legend order
// (window-wide top-N descending, then "other" if any tail collapsed);
// each `points[i].values` is a name → bucket-averaged value map with
// missing names meaning "process wasn't observed in this bucket"
// (Recharts should treat absent as 0 when stacking).
export type ProcessTimelinePoint = {
  at: string;
  values: Record<string, number>;
};

export type ProcessTimelineResponse = {
  pc_id: string;
  metric: string;
  from: string;
  to: string;
  step_seconds: number;
  names: string[];
  points: ProcessTimelinePoint[];
};

// v0.41 / Phase 3: fleet-wide aggregates. Three sibling endpoints
// under /api/perf/* power the Dashboard cards.
export type FleetPerfPoint = {
  at: string;
  value: number | null;
};

export type FleetPerfResponse = {
  metric: string;
  agg: string;
  from: string;
  to: string;
  step_seconds: number;
  points: FleetPerfPoint[];
};

export type TopPerfRow = {
  pc_id: string;
  hostname: string | null;
  value: number;
};

export type TopPerfResponse = {
  metric: string;
  window_seconds: number;
  rows: TopPerfRow[];
};

export type ActiveInvestigation = {
  pc_id: string;
  hostname: string | null;
  latest_at: string;
};

export type ActiveInvestigationsResponse = {
  window_seconds: number;
  rows: ActiveInvestigation[];
};
