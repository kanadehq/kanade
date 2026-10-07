import { describe, expect, test } from 'bun:test';

import { fleetSortValue, isoSortValue } from './inventorySort';

describe('fleetSortValue', () => {
  test('missing values are blank, never zero', () => {
    for (const t of [undefined, 'number', 'bytes', 'timestamp', 'table'] as const) {
      expect(fleetSortValue(null, t)).toBe('');
      expect(fleetSortValue(undefined, t)).toBe('');
      expect(fleetSortValue('', t)).toBe('');
    }
  });
  test('number and bytes sort by the raw number', () => {
    expect(fleetSortValue(1536, 'bytes')).toBe(1536);
    expect(fleetSortValue('2048', 'number')).toBe(2048);
    expect(fleetSortValue('abc', 'number')).toBeUndefined();
  });
  test('timestamp sorts chronologically, junk is blank', () => {
    expect(fleetSortValue('2026-01-02T00:00:00Z', 'timestamp')).toBe(Date.parse('2026-01-02T00:00:00Z'));
    expect(fleetSortValue('nope', 'timestamp')).toBe('');
  });
  test('table sorts by row count', () => {
    expect(fleetSortValue([1, 2, 3], 'table')).toBe(3);
    expect(fleetSortValue('x', 'table')).toBe('');
  });
  test('untyped: numbers numeric, others fall back to text', () => {
    expect(fleetSortValue(7)).toBe(7);
    expect(fleetSortValue('abc')).toBeUndefined();
  });
});

describe('isoSortValue', () => {
  test('parses, blanks on null or junk', () => {
    expect(isoSortValue('2026-01-02T00:00:00Z')).toBe(Date.parse('2026-01-02T00:00:00Z'));
    expect(isoSortValue(null)).toBe('');
    expect(isoSortValue('x')).toBe('');
  });
});
