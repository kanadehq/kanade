// Per-account page-visibility catalog (SPA side of #1008).
//
// These are the **hard-gated** features — the pages the backend enforces via
// `kanade_shared::feature::Feature` + `api::feature_for_path` (403 on a
// disallowed page). For UNRESTRICTED accounts this list is the backend's
// `Feature` catalog MINUS the two always-commons pages:
//   - `dashboard` — the landing feed, reached from the logo.
//   - `agents` — the fleet roster / per-PC detail, shared substrate consumed
//     by Config/Rollout/Groups/Logs too, so it stays baseline-visible.
// A RESTRICTED account (allowed_features is an array) is different: the
// backend 403s it on the commons API routes too (except /api/version,
// /api/command-signing and the /api/auth/* self-service routes), so the SPA
// hides the commons entries and redirects it to its first allowed page —
// such an account reaches literally only what is checked here.
//
// Keep this in lockstep with the Rust `Feature` enum: a page offered here as
// restrictable MUST have a gated route in `feature_for_path`, otherwise the
// SPA would hide a page the backend still serves (soft, not hard).
export const GATEABLE_FEATURES = [
  'run',
  'exec',
  'inventory',
  'compliance',
  'activity',
  'events',
  'audit',
  'logs',
  'collect',
  'analytics',
  'jobs',
  'schedules',
  'views',
  'notifications',
  'rollout',
  'agent-install',
  'apps',
  'remote',
  'groups',
  'config',
  'jetstream',
  'accounts',
  'settings',
] as const;

export type Feature = (typeof GATEABLE_FEATURES)[number];

/** `common.json` `nav.*` key for a feature's human label (reused so the
 *  account editor and the sidebar can't drift on wording). `groups` maps to
 *  `nav.agentGroups` because `nav.groups` is taken by the sidebar section
 *  headings. */
export const FEATURE_NAV_KEY: Record<Feature, string> = {
  run: 'nav.run',
  exec: 'nav.exec',
  inventory: 'nav.inventory',
  compliance: 'nav.compliance',
  activity: 'nav.activity',
  events: 'nav.events',
  audit: 'nav.audit',
  logs: 'nav.logs',
  collect: 'nav.collect',
  analytics: 'nav.analytics',
  jobs: 'nav.jobs',
  schedules: 'nav.schedules',
  views: 'nav.views',
  notifications: 'nav.notifications',
  rollout: 'nav.rollout',
  'agent-install': 'nav.agentInstall',
  apps: 'nav.apps',
  remote: 'nav.remote',
  groups: 'nav.agentGroups',
  config: 'nav.config',
  jetstream: 'nav.jetstream',
  accounts: 'nav.accounts',
  settings: 'nav.settings',
};

// Route-path first segment → gated feature. A route not listed here (`/`,
// `/dashboard`, `/agents`, `/change-password`, unknown) is commons/baseline:
// always reachable for UNRESTRICTED accounts; a restricted account landing on
// one is redirected to its first allowed page by ProtectedLayout (the backend
// 403s it on the commons APIs anyway). Sub-routes inherit their parent
// (`/activity/:id`, `/inventory/search`, `/notifications/:id` → the parent's
// feature) because the guard keys off the first path segment only.
const ROUTE_FEATURE: Record<string, Feature> = {
  '/run': 'run',
  '/exec': 'exec',
  '/inventory': 'inventory',
  '/compliance': 'compliance',
  '/activity': 'activity',
  '/events': 'events',
  '/audit': 'audit',
  '/logs': 'logs',
  '/collect': 'collect',
  '/analytics': 'analytics',
  '/jobs': 'jobs',
  '/schedules': 'schedules',
  '/views': 'views',
  '/notifications': 'notifications',
  '/rollout': 'rollout',
  '/agent-install': 'agent-install',
  '/apps': 'apps',
  '/remote': 'remote',
  '/groups': 'groups',
  '/config': 'config',
  '/jetstream': 'jetstream',
  '/accounts': 'accounts',
  '/settings': 'settings',
};

/** The gated feature owning a route, or `null` for a commons/baseline route
 *  (always reachable). Keys off the first path segment so nested routes
 *  inherit the parent page's feature. */
export function featureForPathname(pathname: string): Feature | null {
  const seg = `/${pathname.split('/')[1] ?? ''}`;
  return ROUTE_FEATURE[seg] ?? null;
}
