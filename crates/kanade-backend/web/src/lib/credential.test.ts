import { describe, expect, test } from 'bun:test';

import en from '../locales/en/agents.json';
import ja from '../locales/ja/agents.json';
import { credentialState, credentialUser, type CredentialState } from './credential';
import type { AgentRow } from './types';

// #1270: the column exists to make one set countable — the hosts still on the
// fleet-wide token. Every case here is a state an operator would act on
// differently, and the first two are the pair that must never merge.

function row(over: Partial<AgentRow> = {}): AgentRow {
  return {
    pc_id: 'PC001',
    hostname: 'PC001',
    os_family: 'windows',
    agent_version: '0.45.3',
    last_heartbeat: '2026-08-02T00:00:00Z',
    updated_at: '2026-08-02T00:00:00Z',
    agent_cpu_pct: null,
    agent_rss_bytes: null,
    agent_disk_read_bytes: null,
    agent_disk_written_bytes: null,
    last_logon_user: null,
    last_logon_display_name: null,
    ...over,
  };
}

describe('credentialState', () => {
  test('a host that was never correlated is unseen, not un-migrated', () => {
    // The distinction the backend column goes out of its way to keep. Reading
    // this as "on the old token" would inflate the migration queue with hosts
    // nobody has heard from — and the fix is upgrading or finding them, not
    // provisioning a credential.
    expect(credentialState(row())).toBe('unseen');
  });

  test('the shared token is its own state — this is the queue', () => {
    expect(credentialState(row({ nats_user: 'shared-token' }))).toBe('shared');
  });

  test('a named user is the end state, whatever it is called', () => {
    // The backend only stores a verbatim value once it has proven the broker
    // deals in usernames, so any unrecognised label IS a username. A fleet
    // that names its users `kanade-agent` and one that names them `agent`
    // must both read as migrated.
    for (const u of ['agent', 'kanade-agent', 'kanade-agent-2026']) {
      expect(credentialState(row({ nats_user: u }))).toBe('named');
    }
  });

  test('a broker that authenticated nobody is not a migrated host', () => {
    // Easy to collapse into `named` by treating "not a sentinel" as a user.
    // A dev broker with no `authorization` block would then read green.
    expect(credentialState(row({ nats_user: 'no-auth' }))).toBe('noAuth');
  });

  test('unproven is separate from unseen: the host IS on the broker', () => {
    // Same colour family, different reason and different next step — one
    // needs the host found, the other needs the backend's own connection
    // looked at. Merging them would hide which.
    expect(credentialState(row({ nats_user: 'unknown' }))).toBe('unproven');
    expect(credentialState(row({ nats_user: 'unknown' }))).not.toBe(credentialState(row()));
  });

  test('an empty string is not a credential', () => {
    // No classifier produces it, but a hand-edited row could hold one, and a
    // blank badge claiming a state is worse than saying nothing is known.
    expect(credentialState(row({ nats_user: '' }))).toBe('unseen');
  });
});

describe('translations', () => {
  // A missing key does not throw — i18next renders the key itself, so the
  // cell would read `credential.unproven` and look like a state rather than a
  // bug. Cheap to pin, and the badge asks for exactly these names.
  const STATES: CredentialState[] = ['named', 'shared', 'noAuth', 'unproven', 'unseen'];

  for (const [lang, bundle] of [
    ['en', en],
    ['ja', ja],
  ] as const) {
    test(`${lang} has a label and a tooltip for every state`, () => {
      const c = (bundle as { credential: Record<string, string> }).credential;
      for (const s of STATES) {
        expect(c[s]).toBeTruthy();
        expect(c[`${s}Title`]).toBeTruthy();
      }
      // The tooltip's timestamp line interpolates, so the placeholder has to
      // survive translation.
      expect(c.since).toContain('{{at}}');
      const cols = (bundle as { columns: Record<string, string> }).columns;
      const titles = (bundle as { columnTitles: Record<string, string> }).columnTitles;
      expect(cols.credential).toBeTruthy();
      expect(titles.credential).toBeTruthy();
    });
  }
});

describe('credentialUser', () => {
  test('shows the username only when there is a real one', () => {
    expect(credentialUser(row({ nats_user: 'kanade-agent' }))).toBe('kanade-agent');
  });

  test('never echoes a sentinel — the badge already says it', () => {
    for (const u of ['shared-token', 'no-auth', 'unknown']) {
      expect(credentialUser(row({ nats_user: u }))).toBeNull();
    }
    expect(credentialUser(row())).toBeNull();
  });
});
