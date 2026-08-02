import { describe, expect, test } from 'bun:test';

import { installerFilename } from './AgentInstall';

// The installer endpoint names its ZIP in Content-Disposition
// (`attachment; filename="kanade-agent-installer-<version>.zip"`). The
// parse must honour that name — it carries the version — and fall back
// to a sane default when the header is missing or malformed.

describe('installerFilename', () => {
  test('parses the quoted filename the backend sends', () => {
    expect(
      installerFilename('attachment; filename="kanade-agent-installer-0.43.99.zip"'),
    ).toBe('kanade-agent-installer-0.43.99.zip');
  });

  test('parses an unquoted filename', () => {
    expect(installerFilename('attachment; filename=kanade-agent-installer.zip')).toBe(
      'kanade-agent-installer.zip',
    );
  });

  test('missing or malformed headers fall back to the default name', () => {
    expect(installerFilename(null)).toBe('kanade-agent-installer.zip');
    expect(installerFilename('attachment')).toBe('kanade-agent-installer.zip');
    expect(installerFilename('attachment; filename=""')).toBe('kanade-agent-installer.zip');
  });
});
