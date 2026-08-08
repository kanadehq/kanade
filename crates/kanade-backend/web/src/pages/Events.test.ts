import { describe, expect, test } from 'bun:test';

import {
  cycleGroup,
  DEFAULT_LIMIT,
  groupState,
  LIMIT_OPTIONS,
  normalizeLimit,
} from './Events';

// Issue #1077: the Events page hydrated `limit` with `Number(raw) || 200`,
// which let any numeric-ish URL value (1e9, -5, 200.5) reach the request.
// The backend 400s those, and the URL-mirror effect wrote the bad value
// straight back — so the error survived a reload with no <Select> option to
// click out of. `normalizeLimit` must clamp every input to one of the four
// offered values, so the page can never be wedged from the URL.

describe('normalizeLimit', () => {
  test('accepts each value the <Select> offers', () => {
    for (const n of LIMIT_OPTIONS) {
      expect(normalizeLimit(String(n))).toBe(n);
    }
  });

  test('missing param falls back to the default', () => {
    expect(normalizeLimit(null)).toBe(DEFAULT_LIMIT);
    expect(normalizeLimit('')).toBe(DEFAULT_LIMIT);
  });

  // The exact reproductions from the issue — each 400s server-side today.
  test('out-of-range and malformed values fall back to the default', () => {
    expect(normalizeLimit('1e9')).toBe(DEFAULT_LIMIT);   // above MAX_LIMIT
    expect(normalizeLimit('-5')).toBe(DEFAULT_LIMIT);    // negative
    expect(normalizeLimit('200.5')).toBe(DEFAULT_LIMIT); // non-integer
    expect(normalizeLimit('0')).toBe(DEFAULT_LIMIT);     // zero
    expect(normalizeLimit('abc')).toBe(DEFAULT_LIMIT);   // NaN
    expect(normalizeLimit('99')).toBe(DEFAULT_LIMIT);    // integer, not offered
  });

  // A value not in the option set must never survive, otherwise the <Select>
  // renders blank and there's no control to recover with — the whole point of
  // clamping on hydration. `100` is numeric, positive, and in-range, yet is
  // not one of the four options, so it must still land on the default.
  test('an in-range value that is not an offered option is rejected', () => {
    expect(LIMIT_OPTIONS as readonly number[]).not.toContain(100);
    expect(normalizeLimit('100')).toBe(DEFAULT_LIMIT);
  });

  test('the default is itself an offered option', () => {
    expect(LIMIT_OPTIONS as readonly number[]).toContain(DEFAULT_LIMIT);
  });
});

// Issue #1342: the chip vocabularies are folded into groups, and a group
// header cycles its whole membership at once. The header is the only
// control that can act on chips the operator cannot currently see (the
// group may be collapsed), so its behaviour has to be exactly
// predictable from what the header itself displays.

describe('groupState', () => {
  const G = ['a', 'b', 'c'];

  test('reports a uniform group as that single state', () => {
    expect(groupState(G, [], [])).toBe('off');
    expect(groupState(G, G, [])).toBe('include');
    expect(groupState(G, [], G)).toBe('exclude');
  });

  test('reports a partially selected group as mixed', () => {
    expect(groupState(G, ['a'], [])).toBe('mixed');
    expect(groupState(G, ['a', 'b'], ['c'])).toBe('mixed');
  });

  // A collapsed group that renders as `off` while holding a selection
  // would be a filter with no visible cause — the operator sees fewer
  // events than they asked for and nothing on screen explains why.
  test('a group holding any selection never reports as off', () => {
    expect(groupState(G, ['b'], [])).not.toBe('off');
    expect(groupState(G, [], ['b'])).not.toBe('off');
  });
});

describe('cycleGroup', () => {
  const G = ['a', 'b'];
  /** Run one cycle and return the resulting lists. */
  const cycle = (inc: string[], exc: string[]) => {
    let nextInc = inc;
    let nextExc = exc;
    cycleGroup(G, inc, exc, (v) => { nextInc = v; }, (v) => { nextExc = v; });
    return { inc: nextInc, exc: nextExc };
  };

  test('follows the same off -> include -> exclude -> off order as a chip', () => {
    const included = cycle([], []);
    expect(included.inc.sort()).toEqual(['a', 'b']);
    expect(included.exc).toEqual([]);

    const excluded = cycle(included.inc, included.exc);
    expect(excluded.inc).toEqual([]);
    expect(excluded.exc.sort()).toEqual(['a', 'b']);

    const off = cycle(excluded.inc, excluded.exc);
    expect(off.inc).toEqual([]);
    expect(off.exc).toEqual([]);
  });

  test('a mixed group enters the cycle at include', () => {
    const r = cycle(['a'], ['b']);
    expect(r.inc.sort()).toEqual(['a', 'b']);
    expect(r.exc).toEqual([]);
  });

  // The bug this rules out: appending to one list without stripping the
  // other leaves a value in BOTH. The backend applies `kinds` and
  // `kinds_ex` independently and the exclusion wins, so such a value
  // would render as an included green chip while actually filtering its
  // own events out — the UI would be stating the opposite of the query.
  test('no value ends up in both lists', () => {
    for (const [inc, exc] of [[[], []], [['a'], ['b']], [['a', 'b'], []], [[], ['a', 'b']]]) {
      const r = cycle(inc, exc);
      expect(r.inc.filter((v) => r.exc.includes(v))).toEqual([]);
    }
  });

  // Group actions must be surgical: a source group's header cannot be
  // allowed to disturb a kind selection, nor one group another's.
  test('values outside the group are left untouched', () => {
    const r = cycle(['other-inc'], ['other-exc']);
    expect(r.inc).toContain('other-inc');
    expect(r.exc).toContain('other-exc');
  });

  test('cycling a group twice from off is not a no-op', () => {
    // Guards against an implementation where include and exclude
    // collapse into the same write.
    const once = cycle([], []);
    const twice = cycle(once.inc, once.exc);
    expect(twice.exc.sort()).toEqual(['a', 'b']);
    expect(twice.inc).toEqual([]);
  });
});
