// Command-signing rollout state for one agent (#1165 / #1253 / #1260).
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

/** The backend's own signing key, from `GET /api/command-signing` (#1260).
 *  Both halves are null when this backend is not signing — a real state, not
 *  an error, which is why the comparison below stays opt-in. */
export type BackendSigningKey = {
  kid: string | null;
  fingerprint: string | null;
};

export type SigningState =
  /** No `command_keys` at all: the agent predates #1195. Upgrade it first —
   *  every other question about this host is unanswerable until then. */
  | 'unknown'
  /** Reports, and holds no keys. The provisioning queue. Harmless today, and
   *  the set that would refuse every command the day enforcement goes on. */
  | 'none'
  /** Holds the backend's current `kid` with DIFFERENT bytes. The state #1229
   *  exists to surface: every command comes back `Invalid`, and because the
   *  key *is* present the reload-on-unknown-key path never fires, so nothing
   *  self-heals. A mistyped paste, a same-kid re-mint, or someone writing to
   *  the trust root all land here. */
  | 'wrongKey'
  /** Holds a ring, but not the backend's current key at all. Expected while a
   *  rotation is still reaching the fleet; a problem once it persists, because
   *  this host rejects everything the backend now signs. */
  | 'staleRing'
  /** Holds keys but cannot say whether it enforces: an agent between #1195 and
   *  #1250. Distinct from `unknown` because a ring is already present — what
   *  is missing is the report, not the keys. */
  | 'keysOnly'
  /** Provisioned and not enforcing. The normal in-progress state: enforcement
   *  begins at the next agent restart, so hosts sit here for days. */
  | 'ready'
  /** Refusing unverified commands. Done. */
  | 'enforcing';

/** Split one reported `kid:fingerprint` entry on the LAST colon, so a `kid`
 *  containing one still parses. Nothing upstream forbids that: the agent joins
 *  the halves and never splits them, so this is the only place the shape is
 *  interpreted. */
function split(entry: string): { kid: string; fp: string | null } {
  const i = entry.lastIndexOf(':');
  if (i < 0) return { kid: entry, fp: null };
  return { kid: entry.slice(0, i), fp: entry.slice(i + 1) };
}

/**
 * Classify one row, comparing against the backend's key when it is known.
 *
 * Order matters, and the ordering IS the judgement:
 *
 *  1. `command_keys` gates everything — without a ring the enforcement answer
 *     cannot be acted on either way.
 *  2. The key comparison outranks `enforcing`. A host enforcing on the wrong
 *     key refuses every command, and painting it green because it reported
 *     `enforcing: true` would be the most misleading cell on the page.
 *
 * `backend` is optional on purpose. Before #1260 the SPA had no expected value
 * and deliberately abstained rather than guess; passing `undefined` — or a
 * backend that is not signing — restores exactly that behaviour instead of
 * inventing a verdict.
 */
export function signingState(a: AgentRow, backend?: BackendSigningKey): SigningState {
  const keys = a.command_keys;
  if (keys === undefined) return 'unknown';
  if (keys.length === 0) return 'none';

  if (backend?.kid && backend.fingerprint) {
    const held = keys.map(split).find((e) => e.kid === backend.kid);
    if (!held) return 'staleRing';
    // A pre-#1229 agent reports a bare kid with no fingerprint. It may well be
    // the right key; nothing here can tell. Fall through rather than accuse —
    // the same abstain-over-guess rule as an unknown backend key.
    if (held.fp !== null && held.fp !== backend.fingerprint) return 'wrongKey';
  }

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
  return (a.command_keys ?? []).map((k) => split(k).kid);
}
