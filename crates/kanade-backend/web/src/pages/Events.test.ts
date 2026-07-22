import { describe, expect, test } from 'bun:test';

import { DEFAULT_LIMIT, LIMIT_OPTIONS, normalizeLimit } from './Events';

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
