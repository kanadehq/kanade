import { describe, expect, test } from 'bun:test';

import {
  groupKinds,
  groupSources,
  groupVocabulary,
  OTHER_GROUP_ID,
  shortLabel,
  type VocabGroup,
} from './vocabGroups';

// The live vocabularies at the time of #1342, captured from
// /api/obs_events/kinds and /api/obs_events/sources on a real fleet.
// Kept verbatim so the grouping is exercised against the shape that
// motivated the issue, not a tidied-up sample.
const REAL_KINDS = [
  'active', 'agent_offline', 'agent_online', 'agent_update', 'app_sample',
  'boot', 'command_signature_absent', 'command_signature_ok',
  'command_signature_unknown_key', 'command_signature_unprovisioned',
  'idle', 'lock', 'log_service_started', 'log_service_stopped', 'logoff',
  'logon', 'presence', 'resume', 'shutdown', 'sleep', 'unexpected_shutdown',
  'unlock', 'web_visit',
];

const REAL_SOURCES = [
  'agent:idle_sampler', 'agent:self_update', 'agent:startup', 'app-usage',
  'attendance-snapshot', 'backend:heartbeat-watchdog', 'command_signature',
  'web-history:brave', 'web-history:chrome', 'web-history:edge',
  'winlog:Security', 'winlog:System',
];

const flatten = (groups: VocabGroup[]) => groups.flatMap((g) => g.values);
const byId = (groups: VocabGroup[], id: string) => groups.find((g) => g.id === id);

/**
 * THE invariant. Every input value must come out in exactly one group —
 * no drops, no duplicates. The whole reason grouping is safe to apply to
 * a vocabulary the SPA does not control is that an unrecognised value
 * degrades to "shown under Other" rather than to "not shown at all": a
 * dropped chip would silently remove an operator's ability to filter by
 * something the fleet is actively emitting.
 */
function everyValueIsGroupedExactlyOnce(input: string[], groups: VocabGroup[]) {
  expect(flatten(groups).sort()).toEqual([...input].sort());
}

describe('groupKinds', () => {
  test('every kind is grouped exactly once', () => {
    everyValueIsGroupedExactlyOnce(REAL_KINDS, groupKinds(REAL_KINDS));
  });

  test('semantic categories capture the values that share no prefix', () => {
    const groups = groupKinds(REAL_KINDS);
    expect(byId(groups, 'session')?.values).toEqual(['lock', 'logoff', 'logon', 'unlock']);
    expect(byId(groups, 'power')?.values).toEqual([
      'boot', 'resume', 'shutdown', 'sleep', 'unexpected_shutdown',
    ]);
    expect(byId(groups, 'presence')?.values).toEqual(['active', 'idle', 'presence']);
    expect(byId(groups, 'activity')?.values).toEqual(['app_sample', 'web_visit']);
  });

  test('prefix families are derived, not tabulated', () => {
    const groups = groupKinds(REAL_KINDS);
    // Named by the LONGEST shared prefix, so the four signature kinds
    // group as "command_signature" and not as the misleading "command".
    expect(byId(groups, 'command_signature')?.values).toEqual([
      'command_signature_absent',
      'command_signature_ok',
      'command_signature_unknown_key',
      'command_signature_unprovisioned',
    ]);
    expect(byId(groups, 'agent')?.values).toEqual([
      'agent_offline', 'agent_online', 'agent_update',
    ]);
    expect(byId(groups, 'log_service')?.values).toEqual([
      'log_service_started', 'log_service_stopped',
    ]);
    expect(byId(groups, 'command')).toBeUndefined();
  });

  test('the real vocabulary needs no Other bucket at all', () => {
    // Not a requirement, but a useful canary: if a future edit to the
    // category table starts dropping known kinds into Other, this fails
    // and says so rather than quietly degrading the grouping.
    expect(byId(groupKinds(REAL_KINDS), OTHER_GROUP_ID)).toBeUndefined();
  });

  test('an unknown kind lands in Other instead of vanishing', () => {
    const withNew = [...REAL_KINDS, 'usb_device_attached', 'defrag'];
    const groups = groupKinds(withNew);
    everyValueIsGroupedExactlyOnce(withNew, groups);
    // `defrag` has no separator at all; `usb_device_attached` has one but
    // no sibling to form a family with. Both are still reachable.
    expect(byId(groups, OTHER_GROUP_ID)?.values).toEqual(['usb_device_attached', 'defrag']);
  });

  test('a new member joins its derived family with no code change', () => {
    const groups = groupKinds([...REAL_KINDS, 'command_signature_expired']);
    expect(byId(groups, 'command_signature')?.values).toContain('command_signature_expired');
    expect(byId(groups, OTHER_GROUP_ID)).toBeUndefined();
  });

  test('a lone underscore value does not invent a group', () => {
    // `_` is word separation in this vocabulary, not a namespace marker.
    // At minGroupSize 1 this would produce a bogus "unexpected" group.
    const groups = groupKinds(['unexpected_shutdown', 'usb_device_attached']);
    expect(byId(groups, 'usb')).toBeUndefined();
    expect(byId(groups, OTHER_GROUP_ID)?.values).toEqual(['usb_device_attached']);
  });

  test('Other is always last so the layout does not reshuffle', () => {
    const groups = groupKinds([...REAL_KINDS, 'defrag']);
    expect(groups[groups.length - 1].id).toBe(OTHER_GROUP_ID);
  });

  test('empty vocabulary produces no groups', () => {
    expect(groupKinds([])).toEqual([]);
  });
});

describe('groupSources', () => {
  test('every source is grouped exactly once', () => {
    everyValueIsGroupedExactlyOnce(REAL_SOURCES, groupSources(REAL_SOURCES));
  });

  test('namespaces fold on the colon', () => {
    const groups = groupSources(REAL_SOURCES);
    expect(byId(groups, 'winlog')?.values).toEqual(['winlog:Security', 'winlog:System']);
    expect(byId(groups, 'web-history')?.values).toEqual([
      'web-history:brave', 'web-history:chrome', 'web-history:edge',
    ]);
    expect(byId(groups, 'agent')?.values).toEqual([
      'agent:idle_sampler', 'agent:self_update', 'agent:startup',
    ]);
  });

  test('a one-member namespace still gets its own group', () => {
    // Unlike `_` in kinds, `:` is an explicit namespace marker — the
    // author of `backend:heartbeat-watchdog` already declared where it
    // belongs, so burying it in Other would discard stated intent.
    expect(byId(groupSources(REAL_SOURCES), 'backend')?.values).toEqual([
      'backend:heartbeat-watchdog',
    ]);
  });

  test('bare sources collect in Other, in the order they arrived', () => {
    expect(byId(groupSources(REAL_SOURCES), OTHER_GROUP_ID)?.values).toEqual([
      'app-usage', 'attendance-snapshot', 'command_signature',
    ]);
  });

  test('a malformed namespace is not mistaken for one', () => {
    // Leading colon (no namespace) and trailing colon (no leaf) are both
    // unusable as groups; they must still be selectable.
    const odd = [':orphan', 'trailing:', 'winlog:Security'];
    const groups = groupSources(odd);
    everyValueIsGroupedExactlyOnce(odd, groups);
    expect(byId(groups, OTHER_GROUP_ID)?.values).toEqual([':orphan', 'trailing:']);
  });
});

describe('groupVocabulary', () => {
  test('a duplicated input value is not rendered twice', () => {
    const groups = groupVocabulary(['winlog:A', 'winlog:A', 'winlog:B'], {
      separator: ':',
      minGroupSize: 1,
    });
    expect(flatten(groups)).toEqual(['winlog:A', 'winlog:B']);
  });

  test('a category claims a value before derivation can', () => {
    // Without tier ordering, `winlog:Security` would form a `winlog`
    // prefix group instead of honouring the explicit table.
    const groups = groupVocabulary(['winlog:Security', 'winlog:System'], {
      categories: [{ id: 'audit', values: ['winlog:Security'] }],
      separator: ':',
      minGroupSize: 1,
    });
    expect(byId(groups, 'audit')?.values).toEqual(['winlog:Security']);
    expect(byId(groups, 'winlog')?.values).toEqual(['winlog:System']);
  });

  test('categories come out in table order, not vocabulary order', () => {
    const groups = groupVocabulary(['b', 'a'], {
      categories: [{ id: 'first', values: ['a'] }, { id: 'second', values: ['b'] }],
      separator: ':',
      minGroupSize: 1,
    });
    expect(groups.map((g) => g.id)).toEqual(['first', 'second']);
  });

  test('a category listing values the fleet never emits is skipped', () => {
    // The table is allowed to name kinds no collector produces here; an
    // empty fold for them would be pure noise.
    const groups = groupVocabulary(['a'], {
      categories: [{ id: 'present', values: ['a'] }, { id: 'ghost', values: ['nope'] }],
      separator: ':',
      minGroupSize: 1,
    });
    expect(groups.map((g) => g.id)).toEqual(['present']);
  });

  test('labelKind distinguishes translatable ids from literal prefixes', () => {
    const groups = groupVocabulary(['x', 'ns:a', 'ns:b'], {
      categories: [{ id: 'known', values: ['x'] }],
      separator: ':',
      minGroupSize: 1,
    });
    expect(byId(groups, 'known')?.labelKind).toBe('category');
    expect(byId(groups, 'ns')?.labelKind).toBe('prefix');
  });

  // PR #1346 review (CodeRabbit + claude, independently). The three tiers
  // draw ids from independent namespaces, so nothing stopped two groups
  // sharing one. `key` is what React and the fold state use, and two
  // groups sharing it expand together and warn — so uniqueness is a
  // property of `key`, asserted directly rather than inferred from `id`.
  test('a derived prefix colliding with the Other bucket keeps a distinct key', () => {
    // `other:foo` derives the prefix id `other`, which is also the
    // catch-all's id.
    const groups = groupSources(['other:foo', 'bare-value']);
    expect(groups.map((g) => g.id)).toContain(OTHER_GROUP_ID);
    expect(new Set(groups.map((g) => g.key)).size).toBe(groups.length);
    everyValueIsGroupedExactlyOnce(['other:foo', 'bare-value'], groups);
  });

  test('a derived prefix colliding with a category keeps a distinct key', () => {
    // A future `session_*` family is a different thing from the literal
    // `logon` / `logoff` the `session` category already claims.
    const input = ['logon', 'logoff', 'session_expired', 'session_renewed'];
    const groups = groupKinds(input);
    expect(groups.filter((g) => g.id === 'session')).toHaveLength(2);
    expect(new Set(groups.map((g) => g.key)).size).toBe(groups.length);
    everyValueIsGroupedExactlyOnce(input, groups);
  });

  test('a category named like the Other bucket keeps a distinct key', () => {
    // The remaining way a category id can collide once derived ids are
    // namespaced: the catch-all's id is a plain word, and nothing stops a
    // future table entry from using it. Without this case the category
    // tier's namespacing is untested — the other collision tests still
    // pass on a build that drops it, because they only pit a category
    // against a DERIVED id.
    const groups = groupVocabulary(['a', 'b'], {
      categories: [{ id: OTHER_GROUP_ID, values: ['a'] }],
      separator: ':',
      minGroupSize: 1,
    });
    expect(groups.filter((g) => g.id === OTHER_GROUP_ID)).toHaveLength(2);
    expect(new Set(groups.map((g) => g.key)).size).toBe(groups.length);
  });

  test('keys are unique across the whole real vocabulary', () => {
    for (const groups of [groupKinds(REAL_KINDS), groupSources(REAL_SOURCES)]) {
      expect(new Set(groups.map((g) => g.key)).size).toBe(groups.length);
    }
  });

  test('a shared prefix never swallows a whole member', () => {
    // `a:b` and `a:b:c` share `a:b`, but naming the group `a:b` would
    // leave the first member with an empty label.
    const groups = groupVocabulary(['a:b', 'a:b:c'], {
      separator: ':',
      minGroupSize: 1,
    });
    expect(groups[0].id).toBe('a');
    expect(flatten(groups)).toEqual(['a:b', 'a:b:c']);
  });
});

// PR #1346 review (CodeRabbit): the caller used to strip the prefix with
// `slice(id.length + 1)`, hardcoding a one-character separator. That holds
// for `_` and `:` and would silently mangle labels for anything longer, so
// the stripping moved here where the separator is known.
describe('shortLabel', () => {
  test('drops the prefix already shown on a derived header', () => {
    const [winlog] = groupSources(['winlog:Security', 'winlog:System']);
    expect(shortLabel(winlog, 'winlog:Security')).toBe('Security');
  });

  test('handles a multi-character separator', () => {
    const [g] = groupVocabulary(['app::a', 'app::b'], { separator: '::', minGroupSize: 1 });
    expect(shortLabel(g, 'app::a')).toBe('a');
  });

  test('leaves category and Other members whole', () => {
    // They share no prefix, so there is nothing redundant to remove and
    // truncating would corrupt the label.
    const groups = groupKinds(['logon', 'defrag']);
    const session = groups.find((g) => g.id === 'session')!;
    const other = groups.find((g) => g.id === OTHER_GROUP_ID)!;
    expect(shortLabel(session, 'logon')).toBe('logon');
    expect(shortLabel(other, 'defrag')).toBe('defrag');
  });

  test('a value that does not carry the prefix is returned unchanged', () => {
    const [winlog] = groupSources(['winlog:Security', 'winlog:System']);
    expect(shortLabel(winlog, 'unrelated')).toBe('unrelated');
  });
});
