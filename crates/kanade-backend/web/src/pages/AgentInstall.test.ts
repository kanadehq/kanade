import { describe, expect, test } from 'bun:test';

import { detectOs, installerFilename, oneLiner } from './AgentInstall';

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

// The one-liner embeds the session token as a Bearer header and points at
// the backend that served the SPA — the exact strings are the contract the
// operator pastes, so pin them verbatim.

describe('oneLiner', () => {
  test('windows: irm | iex against installer.ps1', () => {
    expect(oneLiner('windows', 'https://kanade.example', 'tok123')).toBe(
      "irm -Headers @{Authorization='Bearer tok123'} https://kanade.example/api/agents/installer.ps1 | iex",
    );
  });

  test('linux: token rides a stdin curl config, never argv', () => {
    expect(oneLiner('linux', 'https://kanade.example', 'tok123')).toBe(
      'printf \'header = "Authorization: Bearer %s"\\n\' \'tok123\' | curl -fsSL -K - https://kanade.example/api/agents/installer.sh | sudo bash',
    );
  });

  test('linux: single quotes in the token are shell-escaped', () => {
    expect(oneLiner('linux', 'https://k', "it's")).toContain(`'it'\\''s'`);
  });

  test('linux: double quotes and backslashes are escaped for the curl config', () => {
    // `\` → `\\` then `"` → `\"` (the same rules render_installer_sh uses).
    expect(oneLiner('linux', 'https://k', 'we"ird\\tok')).toContain('we\\"ird\\\\tok');
  });

  test('origin and token are interpolated verbatim', () => {
    const cmd = oneLiner('windows', 'http://localhost:1420', 'dev');
    expect(cmd).toContain('http://localhost:1420/api/agents/installer.ps1');
    expect(cmd).toContain("'Bearer dev'");
  });
});
