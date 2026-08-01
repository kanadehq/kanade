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
