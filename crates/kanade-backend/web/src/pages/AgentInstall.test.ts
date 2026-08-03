import { describe, expect, test } from 'bun:test';

import { detectOs, installerFilename } from './AgentInstall';

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

// The OS toggle preselects the visitor's own platform. userAgentData.platform
// (Chromium) wins over the UA string when both are present; macOS and other
// unsupported platforms fall back to 'windows', the dominant endpoint OS.

describe('detectOs', () => {
  test('Windows UA → windows', () => {
    expect(
      detectOs('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0'),
    ).toBe('windows');
  });

  test('Linux UA (with and without X11) → linux', () => {
    expect(detectOs('Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/131.0.0.0')).toBe(
      'linux',
    );
    expect(detectOs('Mozilla/5.0 (Linux aarch64; rv:133.0) Gecko/20100101 Firefox/133.0')).toBe(
      'linux',
    );
  });

  test('userAgentData.platform takes precedence over the UA string', () => {
    // UA claims Linux, but the (more reliable) client hint says Windows.
    expect(detectOs('Mozilla/5.0 (X11; Linux x86_64)', 'Windows')).toBe('windows');
    expect(detectOs('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 'Linux')).toBe('linux');
  });

  test('macOS and other unrecognized platforms fall back to windows', () => {
    expect(
      detectOs('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15'),
    ).toBe('windows');
    expect(detectOs('', 'macOS')).toBe('windows');
    expect(detectOs('')).toBe('windows');
  });
});
