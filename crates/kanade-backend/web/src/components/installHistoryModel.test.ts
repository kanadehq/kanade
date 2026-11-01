import { describe, expect, test } from 'bun:test';
import { type InstallAttempt, newestAttempts, sortEvents, reconcileSelection, formatTimestamp } from './installHistoryModel';

const attempt = (id: string, created_at: number): InstallAttempt => ({
  id, created_at, expires_at: created_at + 3600, issuer: null, os: 'unix',
  status: 'unknown', last_stage: null, last_outcome: null,
});
const records = [attempt('a', 1), attempt('b', 5), attempt('c', 5)];

describe('installation history model', () => {
  test('sorts newest first with the API id tiebreak without mutating cached data', () => {
    expect(newestAttempts(records).map((a) => a.id)).toEqual(['c', 'b', 'a']);
    expect(records.map((a) => a.id)).toEqual(['a', 'b', 'c']);
  });
  test('selects the newest once while preserving explicit deselection and removed selection', () => {
    expect(reconcileSelection(undefined, [])).toBeUndefined();
    expect(reconcileSelection(undefined, records)).toBe('c');
    expect(reconcileSelection('a', records)).toBe('a');
    expect(reconcileSelection('missing', records)).toBeNull();
    expect(reconcileSelection(null, records)).toBeNull();
  });
  test('orders events by sequence even when receipt times disagree, without mutating the cache', () => {
    const events = [
      { seq: 2, stage: 'complete', outcome: 'succeeded', exit_code: 0, received_at: 1 },
      { seq: 1, stage: 'download', outcome: 'progress', exit_code: 0, received_at: 2 },
    ];
    expect(sortEvents(events).map((e) => e.seq)).toEqual([1, 2]);
    expect(events[0].seq).toBe(2);
  });
  test('formats persisted seconds in the requested locale and local timezone', () => {
    for (const locale of ['en', 'ja']) {
      expect(formatTimestamp(1793400000, locale)).toBe(new Date(1793400000000).toLocaleString(locale));
    }
  });
});
