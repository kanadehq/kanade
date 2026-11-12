import { expect, test } from 'bun:test';
import { locationIsCurrent } from './locationFreshness';

const now = Date.parse('2026-10-18T01:00:00Z');
const fix = { current: true, acquired_at: new Date(now - 110_000).toISOString() };

test('current position expires as time advances, even with a recent refresh', () => {
  expect(locationIsCurrent(fix, now, now, false)).toBe(true);
  expect(locationIsCurrent(fix, now + 11_000, now + 11_000, false)).toBe(false);
});

test('failed refresh or suspended tab demotes a cached current position', () => {
  expect(locationIsCurrent(fix, now, now, true)).toBe(false);
  const fresh = { ...fix, acquired_at: new Date(now).toISOString() };
  expect(locationIsCurrent(fresh, now, now + 16_000, false)).toBe(false);
});

test('server demotion for an offline endpoint cannot be promoted by the UI', () => {
  expect(locationIsCurrent({ ...fix, current: false }, now, now, false)).toBe(false);
  expect(locationIsCurrent(undefined, now, now, false)).toBe(false);
  expect(locationIsCurrent(null, now, now, false)).toBe(false);
  expect(locationIsCurrent({ ...fix, acquired_at: 'invalid' }, now, now, false)).toBe(false);
});
