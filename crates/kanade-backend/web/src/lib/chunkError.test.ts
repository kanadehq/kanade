import { describe, expect, it } from 'bun:test';

import { isChunkLoadError } from './chunkError';

describe('isChunkLoadError', () => {
  it('matches Chromium dynamic-import failures', () => {
    expect(
      isChunkLoadError(
        new TypeError('Failed to fetch dynamically imported module: http://x/assets/Jobs-a.js'),
      ),
    ).toBe(true);
  });

  it('matches Firefox dynamic-import failures', () => {
    expect(
      isChunkLoadError(
        new TypeError('error loading dynamically imported module: http://x/assets/Jobs-a.js'),
      ),
    ).toBe(true);
  });

  it('matches WebKit module-script failures', () => {
    expect(isChunkLoadError(new Error('Importing a module script failed.'))).toBe(true);
  });

  it('matches webpack-style ChunkLoadError by name', () => {
    const e = new Error('Loading chunk 42 failed.');
    e.name = 'ChunkLoadError';
    expect(isChunkLoadError(e)).toBe(true);
  });

  it('rejects ordinary render errors', () => {
    expect(isChunkLoadError(new TypeError('undefined is not an object'))).toBe(false);
    expect(isChunkLoadError(new Error('Network request failed'))).toBe(false);
  });

  it('rejects non-Error values', () => {
    expect(isChunkLoadError('Failed to fetch dynamically imported module')).toBe(false);
    expect(isChunkLoadError(null)).toBe(false);
    expect(isChunkLoadError(undefined)).toBe(false);
  });
});
