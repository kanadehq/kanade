// Command-signing rollout state for one agent (#1165 / #1253).
//
// The Agents page renders one badge per host, and the whole value of that
// badge is that the states it distinguishes need DIFFERENT ACTIONS. Collapsing
// any pair produces a cell that looks informative and tells an operator
// nothing: "upgrade this agent" and "provision this agent" are not the same
// job, and neither is "wait for a reboot".
//
// Kept as a pure function, out of the component, so the mapping is testable
// without a DOM — the `useMemo`-inside-a-component shape is what made an
// earlier rollout view untestable (#1086).

import type { AgentRow } from './types';

export type SigningState =
  /** No `command_keys` at all: the agent predates #1195. Upgrade it first —
   *  every other question about this host is unanswerable until then. */
  | 'unknown'
  /** Reports, and holds no keys. The provisioning queue. Harmless today, and
   *  the set that would refuse every command the day enforcement goes on. */
  | 'none'
  /** Holds keys but cannot say whether it enforces: an agent between #1195 and
   *  #1250. Distinct from `unknown` because a ring is already present — what
   *  is missing is the report, not the keys. */
  | 'keysOnly'
  /** Provisioned and not enforcing. The normal in-progress state: enforcement
   *  begins at the next agent restart, so hosts sit here for days. */
  | 'ready'
  /** Refusing unverified commands. Done. */
  | 'enforcing';

/**
 * Classify one row. Order matters: `command_keys` gates everything, because
 * without a ring the enforcement answer cannot be acted on either way.
 *
 * Deliberately does NOT try to judge whether the ring is the *right* one. That
 * needs the backend's own `kid:fingerprint` to compare against (#1229), which
 * no API exposes today — and guessing would be worse than abstaining, since a
 * host with the correct id and wrong bytes refuses everything while looking
 * perfect here.
 */
export function signingState(a: AgentRow): SigningState {
  const keys = a.command_keys;
  if (keys === undefined) return 'unknown';
  if (keys.length === 0) return 'none';
  if (a.enforcing === undefined) return 'keysOnly';
  return a.enforcing ? 'enforcing' : 'ready';
}

/**
 * The `kid` half of each entry, for a tooltip. The fingerprint is what makes
 * two rings comparable across machines, but it is 16 hex characters nobody
 * reads at a glance — so the tooltip leads with ids and keeps the full string
 * available underneath.
 */
export function keyIds(a: AgentRow): string[] {
  return (a.command_keys ?? []).map((k) => k.split(':')[0] ?? k);
}
