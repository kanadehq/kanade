import { describe, expect, test } from 'bun:test';

import en from '../locales/en/passkey.json';
import ja from '../locales/ja/passkey.json';
import {
  b64urlToBytes,
  bytesToB64url,
  isCancellation,
  passkeysSupported,
  toCreationOptions,
  toRequestOptions,
} from './passkey';

describe('base64url', () => {
  test('round-trips every byte value, with no padding or unsafe characters', () => {
    const all = Uint8Array.from({ length: 256 }, (_, i) => i);
    const s = bytesToB64url(all);
    expect(s).not.toMatch(/[+/=]/);
    expect([...b64urlToBytes(s)]).toEqual([...all]);
  });

  test('handles every remainder length (0, 1, 2 mod 3)', () => {
    for (let n = 0; n < 8; n++) {
      const b = Uint8Array.from({ length: n }, (_, i) => 250 - i);
      expect([...b64urlToBytes(bytesToB64url(b))]).toEqual([...b]);
    }
  });

  test('decodes the url-safe alphabet', () => {
    // 0xfb 0xff -> "-_8" in url-safe, "+/8=" in standard base64.
    expect([...b64urlToBytes('-_8')]).toEqual([0xfb, 0xff]);
  });

  test('accepts an ArrayBuffer as well as a view', () => {
    const b = Uint8Array.from([1, 2, 3]);
    expect(bytesToB64url(b.buffer)).toBe(bytesToB64url(b));
  });
});

describe('option mapping', () => {
  test('creation options get binary challenge / user.id / excludeCredentials', () => {
    const o = toCreationOptions({
      publicKey: {
        rp: { id: 'example.com', name: 'Kanade' },
        challenge: 'AQID',
        user: { id: 'BAUG', name: 'alice', displayName: 'alice' },
        excludeCredentials: [{ type: 'public-key', id: 'BwgJ' }],
        authenticatorSelection: { residentKey: 'required', userVerification: 'required' },
      },
    });
    const pk = o.publicKey!;
    expect([...new Uint8Array(pk.challenge as ArrayBuffer)]).toEqual([1, 2, 3]);
    expect([...new Uint8Array(pk.user.id as ArrayBuffer)]).toEqual([4, 5, 6]);
    expect([...new Uint8Array(pk.excludeCredentials![0].id as ArrayBuffer)]).toEqual([7, 8, 9]);
    // Everything the server asked for (UV, resident key) is passed through.
    expect(pk.authenticatorSelection).toEqual({
      residentKey: 'required',
      userVerification: 'required',
    });
    expect(pk.rp.id).toBe('example.com');
  });

  test('request options keep UV and never request autofill mediation', () => {
    const o = toRequestOptions({
      publicKey: { challenge: 'AQID', rpId: 'example.com', userVerification: 'required' },
    });
    expect([...new Uint8Array(o.publicKey!.challenge as ArrayBuffer)]).toEqual([1, 2, 3]);
    expect(o.publicKey!.userVerification).toBe('required');
    expect(o.publicKey!.allowCredentials).toBeUndefined();
    expect(o.mediation).toBeUndefined();
  });
});

describe('error classification', () => {
  test('a dismissed or timed-out prompt is a cancellation, not an error', () => {
    expect(isCancellation({ name: 'NotAllowedError' })).toBe(true);
    expect(isCancellation({ name: 'AbortError' })).toBe(true);
    expect(isCancellation(new DOMException('x', 'NotAllowedError'))).toBe(true);
  });

  test('anything else is a real failure', () => {
    expect(isCancellation({ name: 'SecurityError' })).toBe(false);
    expect(isCancellation({ name: 'InvalidStateError' })).toBe(false);
    expect(isCancellation(new Error('boom'))).toBe(false);
    expect(isCancellation(null)).toBe(false);
    expect(isCancellation('NotAllowedError')).toBe(false);
  });
});

describe('support detection', () => {
  test('a runtime without the WebAuthn API is reported as unsupported', () => {
    // bun test has no window/PublicKeyCredential.
    expect(passkeysSupported()).toBe(false);
  });
});

describe('locale catalogue', () => {
  test('en and ja carry the same keys for the passkey namespace', () => {
    const keys = (o: Record<string, unknown>) => Object.keys(o).sort();
    expect(keys(ja)).toEqual(keys(en));
  });
});
