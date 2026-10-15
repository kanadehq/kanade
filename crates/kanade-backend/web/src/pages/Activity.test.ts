import { describe, expect, test } from 'bun:test';

import type { Role } from '../lib/auth';
import type { Feature } from '../lib/features';
import { killIsOffered } from './Activity';

// Who the Activity page offers its per-row stop to. The backend gates
// `POST /api/jobs/{job_id}/kill` under `Feature::Jobs` (the route table) and
// `Role::Operator` (the vertical gate), while the page itself is reachable by
// an `activity`-only account — so the button has to clear both, or it is a
// button whose click is a 403 toast. Regression: it used to be rendered for
// anyone who could see the page.
const RANK: Record<Role, number> = { viewer: 0, operator: 1, admin: 2 };

/** A stand-in for `useAuth()`, with the same two predicates. `features` is
 *  `null` for an unrestricted account, as in the real context. */
function account(role: Role, features: Feature[] | null) {
  return {
    hasRole: (min: Role) => RANK[role] >= RANK[min],
    canSee: (feature: Feature) => features === null || features.includes(feature),
  };
}

const RUNNING = { job_id: 'remediate', finished_at: null };
const FINISHED = { job_id: 'remediate', finished_at: '2026-01-01T00:00:00Z' };
const AD_HOC = { job_id: null, finished_at: null };

describe('killIsOffered', () => {
  test('an unrestricted operator gets it', () => {
    expect(killIsOffered(RUNNING, account('operator', null))).toBe(true);
  });

  test('an admin gets it', () => {
    expect(killIsOffered(RUNNING, account('admin', null))).toBe(true);
  });

  test('an operator holding jobs gets it', () => {
    expect(killIsOffered(RUNNING, account('operator', ['jobs']))).toBe(true);
  });

  test('an operator holding jobs alongside activity still gets it', () => {
    expect(killIsOffered(RUNNING, account('operator', ['activity', 'jobs']))).toBe(true);
  });

  test('an activity-only operator does not — the route is Jobs-gated', () => {
    expect(killIsOffered(RUNNING, account('operator', ['activity']))).toBe(false);
  });

  test('a jobs-holding viewer does not — the route is operator-gated', () => {
    expect(killIsOffered(RUNNING, account('viewer', ['jobs']))).toBe(false);
  });

  test('an unrestricted viewer does not', () => {
    expect(killIsOffered(RUNNING, account('viewer', null))).toBe(false);
  });

  test('a finished row has nothing to kill', () => {
    expect(killIsOffered(FINISHED, account('operator', null))).toBe(false);
  });

  test('an ad-hoc run row carries no job_id', () => {
    expect(killIsOffered(AD_HOC, account('operator', null))).toBe(false);
  });
});
