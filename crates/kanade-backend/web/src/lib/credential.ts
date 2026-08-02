// Which NATS credential one agent's live connection authenticated with
// (#1270), as the Agents page needs to render it.
//
// Same discipline as `signing.ts`, and for the same reason: a badge is only
// worth the column if the states it separates need DIFFERENT ACTIONS. The pair
// that must never collapse here is "still on the shared token" (the migration
// queue — provision this host) and "never correlated" (nothing is known —
// upgrade or investigate it). Painting both grey would make the queue
// uncountable, which is the whole point of the field.
//
// Pure function, out of the component, so the mapping is testable without a
// DOM (#1086).

import type { AgentRow } from './types';

/** What the backend stores for a token-authenticated connection. It never
 *  stores the credential itself: nats-server hides it, and even when a broker
 *  build does not, the projector refuses to record a value it cannot prove is
 *  safe. */
const SHARED = 'shared-token';
/** The broker authenticated nobody — no `authorization` block at all. */
const NO_AUTH = 'no-auth';
/** Connected, but the credential cannot be named without risking printing a
 *  secret. Distinct from "not seen": the host IS on the broker. */
const UNPROVEN = 'unknown';

export type CredentialState =
  /** No live connection has been correlated to this host: either it has not
   *  been seen since the backend started polling, or its agent predates #1270
   *  and announces no pc_id to join on. Unknown — NOT "on the old
   *  credential", and not something to provision. */
  | 'unseen'
  /** Authenticated with the fleet-wide token. **The migration queue**: the set
   *  that has to reach zero before #1266 can narrow anything. */
  | 'shared'
  /** The broker required no credential at all. Expected on a dev broker,
   *  alarming anywhere else — and not a state a host can fix on its own. */
  | 'noAuth'
  /** On the broker, but the backend cannot vouch for what it authenticated as
   *  (see `Evidence` in the projector). Nothing to provision here; the gap is
   *  in what can be proven, not in what the host holds. */
  | 'unproven'
  /** Authenticated as a named NATS user. The end state of the migration. */
  | 'named';

/**
 * Classify one row.
 *
 * Deliberately total over the string space: any value that is not one of the
 * three sentinels IS a username, because the backend only ever stores a
 * verbatim value when it has proven the broker deals in usernames. So the
 * default arm is `named` rather than a fallback state — an unrecognised label
 * here would mean the projector changed, not that the host is in trouble.
 */
export function credentialState(a: AgentRow): CredentialState {
  const u = a.nats_user;
  if (u === undefined || u === '') return 'unseen';
  if (u === SHARED) return 'shared';
  if (u === NO_AUTH) return 'noAuth';
  if (u === UNPROVEN) return 'unproven';
  return 'named';
}

/**
 * The NATS username to show beside the badge, when there is one.
 *
 * Only for `named`: the sentinels are this SPA's vocabulary, not the broker's,
 * and echoing `shared-token` next to a badge that already says so is noise.
 */
export function credentialUser(a: AgentRow): string | null {
  return credentialState(a) === 'named' ? (a.nats_user ?? null) : null;
}
