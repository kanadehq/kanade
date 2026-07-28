import { describe, expect, test } from 'bun:test';

import { validateSupportCode } from './supportCodes';

// #1166 follow-up: the support-code form mirrors the backend's
// `validate_support_code` so an operator sees why a draft is rejected without
// a round-trip. Each rule exists because breaking it produces a code that is
// silently unusable rather than an error:
//
//   * scope — compared BYTE-FOR-BYTE with a job's `client.unlock`, so a slug
//             outside [A-Za-z0-9._-] can never match one and the job stays
//             hidden forever with nothing logged anywhere.
//   * code  — must not be trimmed (whitespace inside a secret is significant)
//             yet must not CARRY edge whitespace either, since the Client App
//             trims what the user types: such a code could be stored and then
//             never redeemed.
//   * ttl   — 0 would mint an already-expired grant, i.e. an unlock that
//             appears to work and reveals nothing.

const ok = { scope: 'support', code: 'hunter2!!', label: '', ttlMinutes: '' };

describe('validateSupportCode', () => {
  test('accepts a minimal valid draft', () => {
    expect(validateSupportCode(ok)).toBeNull();
  });

  test('accepts the optional fields when filled', () => {
    expect(validateSupportCode({ ...ok, label: 'ヘルプデスク', ttlMinutes: '30' })).toBeNull();
  });

  test('scope must be a slug', () => {
    for (const scope of ['', '   ', 'help desk', 'help/desk', 'help:desk', 'ヘルプ']) {
      expect(validateSupportCode({ ...ok, scope })).toBe('scope');
    }
    // The charset itself, all four punctuation marks included.
    for (const scope of ['support', 'admin', 'a.b_c-d', 'Tier1']) {
      expect(validateSupportCode({ ...ok, scope })).toBeNull();
    }
  });

  test('scope is accepted with surrounding whitespace (the backend trims it)', () => {
    // Deliberately NOT an error: the endpoint trims the scope before storing,
    // so a padded slug still lands as the clean one. Only the CODE has to
    // reject padding.
    expect(validateSupportCode({ ...ok, scope: '  support  ' })).toBeNull();
  });

  test('code must be at least 8 characters', () => {
    expect(validateSupportCode({ ...ok, code: '' })).toBe('codeShort');
    expect(validateSupportCode({ ...ok, code: 'short12' })).toBe('codeShort');
    expect(validateSupportCode({ ...ok, code: 'exactly8' })).toBeNull();
  });

  test('code length counts characters, not UTF-16 units', () => {
    // Four emoji are 8 UTF-16 units but only 4 characters — `.length` would
    // wave this through as long enough, and the backend (which counts chars)
    // would then reject it.
    expect(validateSupportCode({ ...ok, code: '🐇🐇🐇🐇' })).toBe('codeShort');
    expect(validateSupportCode({ ...ok, code: '🐇🐇🐇🐇🐇🐇🐇🐇' })).toBeNull();
  });

  test('code rejects leading / trailing whitespace but allows it inside', () => {
    expect(validateSupportCode({ ...ok, code: ' hunter2!!' })).toBe('codeWhitespace');
    expect(validateSupportCode({ ...ok, code: 'hunter2!! ' })).toBe('codeWhitespace');
    expect(validateSupportCode({ ...ok, code: '\thunter2!!' })).toBe('codeWhitespace');
    // A passphrase with interior spaces is a perfectly good code.
    expect(validateSupportCode({ ...ok, code: 'correct horse battery' })).toBeNull();
  });

  test('ttl must be a whole number in 1..=480 when given', () => {
    expect(validateSupportCode({ ...ok, ttlMinutes: '' })).toBeNull();
    expect(validateSupportCode({ ...ok, ttlMinutes: '   ' })).toBeNull();
    expect(validateSupportCode({ ...ok, ttlMinutes: '1' })).toBeNull();
    expect(validateSupportCode({ ...ok, ttlMinutes: '480' })).toBeNull();
    // '1e3' is what a number input can hand back for exponent notation:
    // `Number` accepts it (1000), so only the range check catches it.
    for (const ttlMinutes of ['0', '-1', '481', '1e3', '15.5', 'abc', 'Infinity']) {
      expect(validateSupportCode({ ...ok, ttlMinutes })).toBe('ttl');
    }
  });

  test('label may be blank but not whitespace-only', () => {
    expect(validateSupportCode({ ...ok, label: '' })).toBeNull();
    expect(validateSupportCode({ ...ok, label: '  ' })).toBe('label');
    expect(validateSupportCode({ ...ok, label: 'desk' })).toBeNull();
  });

  test('scope is reported before the code when both are wrong', () => {
    // Field order matters for the single-message form: the operator fixes the
    // first field, not a random one.
    expect(validateSupportCode({ scope: '', code: '', label: '', ttlMinutes: '' })).toBe('scope');
  });
});
