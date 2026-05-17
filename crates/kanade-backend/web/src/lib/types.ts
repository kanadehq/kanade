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
  inventory_interval?: string;
  inventory_jitter?: string;
  inventory_enabled?: boolean;
  heartbeat_interval?: string;
};

export type EffectiveConfig = {
  target_version: string | null;
  inventory_interval: string;
  inventory_jitter: string;
  inventory_enabled: boolean;
  heartbeat_interval: string;
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
