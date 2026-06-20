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
  // #655: the account the host's Windows sign-in screen last used —
  // `last_logon_user` is the DOMAIN\\sam login name,
  // `last_logon_display_name` its friendly name. Null for
  // never-signed-in / pre-#655 / non-Windows agents.
  last_logon_user: string | null;
  last_logon_display_name: string | null;
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
  // Operator-facing product name the end-user Client App displays
  // (window title / header / Start-Menu shortcut / toast sender).
  // See [`ConfigScope::client_display_name`] on the Rust side.
  client_display_name?: string;
};

export type EffectiveConfig = {
  target_version: string | null;
  target_version_jitter: string;
  heartbeat_interval: string;
  host_perf_interval: string;
  process_perf_enabled: boolean;
  process_perf_expires_at: string | null;
  process_perf_top_n: number;
  client_display_name: string | null;
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

// ---- Phase E notifications (mirror kanade-shared ipc::notifications) ----

export type NotificationPriority = 'info' | 'warn' | 'emergency';

// Request body for POST /api/notifications. `id` is optional — the
// backend mints a UUID when omitted; `target` is the same shape as a
// job manifest's `target` (at least one of all/groups/pcs must be set).
export type PublishNotificationRequest = {
  id?: string;
  priority: NotificationPriority;
  require_ack: boolean;
  title: string;
  body: string;
  // Surface an OS toast on the Client App — decoupled from priority.
  // true = persistent toast (+ launches a closed app, lock screen / Action
  // Center); false = in-app list only. Defaults to false when omitted.
  toast: boolean;
  issued_by?: string;
  expires_at?: string;
  target: { all: boolean; groups: string[]; pcs: string[] };
};

export type PublishNotificationResponse = {
  id: string;
  subjects: string[];
};

// One recipient's confirmation, from GET /api/notifications/{id}/ack_status.
// `account` is a human-readable label for who confirmed (the acking user's
// login, or the PC's last-logon as a fallback). Absent (not null — the Rust
// side skips a None field on the wire) when unavailable, in which case the
// UI falls back to `user_sid`.
export type NotificationAckEntry = {
  pc_id: string;
  user_sid: string;
  acked_at: string;
  account?: string;
};

export type NotificationAckStatus = {
  id: string;
  acks: NotificationAckEntry[];
};

// One sent notification, from GET /api/notifications (the operator's
// sent history). Mirrors kanade-shared `ipc::notifications::Notification`
// — `acked_at` is per-recipient and always absent here (the operator
// view is "what was sent", not "did I personally ack it").
export type NotificationRecord = {
  id: string;
  priority: NotificationPriority;
  require_ack: boolean;
  title: string;
  body: string;
  toast: boolean;
  issued_at: string;
  issued_by?: string | null;
  expires_at?: string | null;
  // Set when the notification was edited in place (PATCH); drives the
  // "edited" badge. Absent (not null) on the wire when never edited.
  edited_at?: string | null;
  acks_reset_at?: string | null;
};

// Request body for PATCH /api/notifications/{id} — edit a sent notification's
// content. Audience is immutable (no `target`); `reset_acks` forces re-confirm
// of a materially-changed body. The SPA submits the full editable set.
export type EditNotificationRequest = {
  priority: NotificationPriority;
  require_ack: boolean;
  title: string;
  body: string;
  toast: boolean;
  expires_at?: string;
  reset_acks: boolean;
};

// One targeted PC's confirmation state (④), from the detail endpoint's
// `audience` roster. PC granularity: `confirmed` is true once any user on
// the PC acked; `last_logon_*` is the host's representative user.
// All optionals are absent (not null) on the wire — the Rust side skips a
// None field — so `?: T` mirrors the contract; `| null` would be dead.
export type AudiencePc = {
  pc_id: string;
  last_logon_user?: string;
  last_logon_display_name?: string;
  confirmed: boolean;
  acked_at?: string;
};

// The original send target (where it was addressed), reconstructed from
// the fan-out subjects — the operator's intent, vs the resolved per-PC
// `audience` roster.
export type NotificationTarget = {
  all: boolean;
  groups: string[];
  pcs: string[];
};

// One sent notification's full content + its confirmation list + the
// per-PC audience roster, from GET /api/notifications/{id}. Powers the
// deep-linkable detail page.
export type NotificationDetail = {
  notification: NotificationRecord;
  acks: NotificationAckEntry[];
  audience: AudiencePc[];
  target?: NotificationTarget;
};

// Router state carried from a history row's "reuse" action into the
// composer (NOT the wire — the audience isn't stored on the notification,
// so the operator re-picks the target after a reuse).
export type NotificationReuse = {
  reuse: {
    priority: NotificationPriority;
    require_ack: boolean;
    title: string;
    body: string;
    toast: boolean;
    issued_by?: string | null;
  };
};
