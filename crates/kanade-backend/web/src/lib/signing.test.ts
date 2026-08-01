import { describe, expect, test } from 'bun:test';

import { keyIds, signingState } from './signing';
import type { AgentRow } from './types';

// #1253: the badge on the Agents page is only worth rendering if the states it
// separates need different actions. Each case below is a state an operator
// would respond to differently, and the pairs that are easiest to collapse are
// the ones this file exists to pin.

function row(over: Partial<AgentRow> = {}): AgentRow {
  return {
    pc_id: 'PC001',
    hostname: 'PC001',
    os_family: 'windows',
    agent_version: '0.45.2',
    last_heartbeat: '2026-08-01T00:00:00Z',
    updated_at: '2026-08-01T00:00:00Z',
    agent_cpu_pct: null,
    agent_rss_bytes: null,
    agent_disk_read_bytes: null,
    agent_disk_written_bytes: null,
    last_logon_user: null,
    last_logon_display_name: null,
    ...over,
  };
}

const RING = ['backend-20260728:75b4c8f44e18012d', 'break-glass-20260730-2238:58dda849ae14f064'];

describe('signingState', () => {
  test('an agent that never reported is unknown, not unprovisioned', () => {
    // The distinction the wire format goes out of its way to keep. Rendering
    // these as "no keys" would send an operator to provision a machine that
    // cannot accept the answer yet — it needs upgrading first.
    expect(signingState(row())).toBe('unknown');
  });

  test('an empty ring is the provisioning queue', () => {
    expect(signingState(row({ command_keys: [] }))).toBe('none');
  });

  test('keys without an enforcement report is its own state', () => {
    // Every 0.45.1 agent in the fleet right now: `command_keys` shipped in
    // #1195/#1229, `enforcing` only in #1250. A ring is already present, so
    // this must NOT read as "unknown" — the action is "upgrade to see", not
    // "provision". (Whether that ring is the RIGHT one is a question nothing
    // here can answer; see the note on `signingState`.)
    expect(signingState(row({ command_keys: RING }))).toBe('keysOnly');
  });

  test('provisioned but not enforcing is the normal in-progress state', () => {
    // Where most hosts sit for days: enforcement begins at the next agent
    // restart, not when the enable job runs.
    expect(signingState(row({ command_keys: RING, enforcing: false }))).toBe('ready');
  });

  test('enforcing is the only done state', () => {
    expect(signingState(row({ command_keys: RING, enforcing: true }))).toBe('enforcing');
  });

  test('an enforcing claim without a ring cannot outrank the missing ring', () => {
    // Not reachable from a real heartbeat — both fields come from one
    // observation under one lock (#1250) — but the ordering has to be
    // deliberate rather than accidental. An empty ring means the agent
    // declines to enforce, so "no keys" is the honest answer even if a stale
    // projection paired it with `true`.
    expect(signingState(row({ command_keys: [], enforcing: true }))).toBe('none');
    expect(signingState(row({ enforcing: true }))).toBe('unknown');
  });
});

describe('signingState with the backend key known (#1260)', () => {
  const BACKEND = { kid: 'backend-20260728', fingerprint: '75b4c8f44e18012d' };

  test('a matching ring still reaches the enforcement states', () => {
    expect(signingState(row({ command_keys: RING, enforcing: true }), BACKEND)).toBe('enforcing');
    expect(signingState(row({ command_keys: RING, enforcing: false }), BACKEND)).toBe('ready');
  });

  test('the right kid with wrong bytes outranks an enforcing claim', () => {
    // The most important ordering in this file. A host enforcing on the wrong
    // key refuses EVERY command, and reporting `enforcing: true` is exactly
    // what it does while doing so — so if `enforcing` were checked first the
    // page would paint the worst host on the fleet green.
    const wrong = ['backend-20260728:0000000000000000', RING[1]!];
    expect(signingState(row({ command_keys: wrong, enforcing: true }), BACKEND)).toBe('wrongKey');
    expect(signingState(row({ command_keys: wrong, enforcing: false }), BACKEND)).toBe('wrongKey');
  });

  test('a ring without the current kid at all is stale, not wrong', () => {
    // Different cause, different urgency: this is the ordinary state of a
    // machine a rotation has not reached yet, while `wrongKey` means someone
    // wrote bytes nobody intended.
    const old = ['backend-20260101:aaaaaaaaaaaaaaaa', RING[1]!];
    expect(signingState(row({ command_keys: old, enforcing: true }), BACKEND)).toBe('staleRing');
  });

  test('a backend that is not signing restores the abstaining behaviour', () => {
    // Null halves are a real state, not an error. With nothing to compare
    // against, a verdict would be invented — so the states collapse back to
    // what the agent reported, exactly as before #1260.
    const none = { kid: null, fingerprint: null };
    const wrong = ['backend-20260728:0000000000000000'];
    expect(signingState(row({ command_keys: wrong, enforcing: true }), none)).toBe('enforcing');
    expect(signingState(row({ command_keys: wrong, enforcing: true }), undefined)).toBe('enforcing');
  });

  test('a bare kid from a pre-fingerprint agent is not accused', () => {
    // #1229 added the fingerprint half; an older agent reports the kid alone.
    // It may hold the right key — nothing here can tell — so abstain rather
    // than flag a machine that is probably fine.
    expect(signingState(row({ command_keys: ['backend-20260728'], enforcing: true }), BACKEND)).toBe(
      'enforcing',
    );
  });

  test('an empty ring stays the provisioning queue, not a mismatch', () => {
    expect(signingState(row({ command_keys: [] }), BACKEND)).toBe('none');
    expect(signingState(row(), BACKEND)).toBe('unknown');
  });

  test('a kid containing a colon still compares on the right halves', () => {
    // Split on the LAST colon: nothing forbids a colon in a kid, and getting
    // this wrong would compare "backend" against "2026:75b4…" and accuse every
    // host in the fleet at once.
    const backend = { kid: 'backend:eu', fingerprint: '75b4c8f44e18012d' };
    const held = ['backend:eu:75b4c8f44e18012d'];
    expect(signingState(row({ command_keys: held, enforcing: true }), backend)).toBe('enforcing');
  });
});

describe('keyIds', () => {
  test('strips the fingerprint so a tooltip leads with names', () => {
    expect(keyIds(row({ command_keys: RING }))).toEqual([
      'backend-20260728',
      'break-glass-20260730-2238',
    ]);
  });

  test('survives an entry with no fingerprint half', () => {
    // A pre-#1229 agent reports bare kids. Splitting on ':' still yields the
    // kid, so a mixed fleet mid-upgrade renders rather than showing blanks.
    expect(keyIds(row({ command_keys: ['backend-20260728'] }))).toEqual(['backend-20260728']);
  });

  test('an absent ring yields nothing rather than throwing', () => {
    expect(keyIds(row())).toEqual([]);
  });
});
