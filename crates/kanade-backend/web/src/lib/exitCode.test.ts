import { describe, expect, test } from 'bun:test';

import { exitTone } from './exitCode';

describe('exitTone', () => {
  test('separates clean, skipped and failed runs', () => {
    expect(exitTone(0, false)).toBe('success');
    expect(exitTone(1, false)).toBe('danger');
    expect(exitTone(125, true)).toBe('skipped');
  });

  // The flag decides, never the exit code: sh exits 126 / 127 for
  // "not executable" / "command not found", and the agent publishes its
  // signature refusal (123) unflagged — all of them are failures.
  test('an unflagged reserved exit code is a failure, not a skip', () => {
    expect(exitTone(123, false)).toBe('danger');
    expect(exitTone(126, false)).toBe('danger');
    expect(exitTone(127, false)).toBe('danger');
  });

  test('a result from an agent that predates the flag reads by exit code', () => {
    expect(exitTone(0, undefined)).toBe('success');
    expect(exitTone(124, undefined)).toBe('danger');
  });
});
