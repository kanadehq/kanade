import { describe, expect, test } from 'bun:test';

import { releaseBaseVersion } from './Rollout';

describe('releaseBaseVersion', () => {
  test('bare keys pass through unchanged', () => {
    expect(releaseBaseVersion('0.45.4')).toBe('0.45.4');
    expect(releaseBaseVersion('0.46.0-rc.1+build.5')).toBe('0.46.0-rc.1+build.5');
  });

  test('linux platform suffixes are stripped', () => {
    expect(releaseBaseVersion('0.45.4-linux-x86_64')).toBe('0.45.4');
    expect(releaseBaseVersion('0.45.4-linux-aarch64')).toBe('0.45.4');
  });

  test('prerelease dashes are not platform suffixes', () => {
    // Only the exact trailing suffix counts — a prerelease mentioning
    // linux is still a base version.
    expect(releaseBaseVersion('1.0.0-linux')).toBe('1.0.0-linux');
    expect(releaseBaseVersion('0.46.0-rc.1-linux-x86_64')).toBe('0.46.0-rc.1');
  });
});
