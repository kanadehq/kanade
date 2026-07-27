import { describe, expect, it } from 'bun:test';
import {
  decodeFrame,
  FrameDecodeError,
  remoteSocketUrl,
  subprotocols,
  SUBPROTOCOL,
  type FrameMeta,
  type TileMeta,
} from './remoteFrame';

/** Build a socket message the way `api::remote::frame` does. */
function frame(meta: unknown, payload: number[] = []): ArrayBuffer {
  const json = new TextEncoder().encode(JSON.stringify(meta));
  const out = new Uint8Array(4 + json.length + payload.length);
  new DataView(out.buffer).setUint32(0, json.length, true);
  out.set(json, 4);
  out.set(payload, 4 + json.length);
  return out.buffer;
}

const TILE: TileMeta = {
  kind: 'tile',
  frame_seq: 7,
  tile_index: 1,
  tile_count: 4,
  x: 0,
  y: 400,
  w: 3840,
  h: 400,
  screen_w: 3840,
  screen_h: 1600,
  captured_at_ms: 1_700_000_000_000,
  encoding: 'jpeg',
};

describe('decodeFrame', () => {
  it('splits meta from payload on the length prefix', () => {
    const jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00];
    const { meta, payload } = decodeFrame(frame(TILE, jpeg));
    expect(meta).toEqual(TILE);
    // The prefix covers the meta only, so the payload is the rest — byte for
    // byte, including the JPEG magic the decoder needs.
    expect(Array.from(payload)).toEqual(jpeg);
  });

  it('reads the prefix as little-endian', () => {
    // A meta of 260 bytes is 0x00000104: big-endian would read 0x04010000 and
    // blow past the buffer. Pad the JSON out to exactly that length so the
    // two interpretations cannot both work.
    const pad = 'x'.repeat(260 - JSON.stringify({ kind: 'gap', reason: '' }).length);
    const buf = frame({ kind: 'gap', reason: pad });
    expect(new DataView(buf).getUint32(0, true)).toBe(260);
    const { meta } = decodeFrame(buf);
    expect(meta.kind).toBe('gap');
  });

  it('yields an empty payload for the non-tile kinds', () => {
    const metas: FrameMeta[] = [
      { kind: 'started', session_id: 'sess-1', screen_w: null, screen_h: null, allow_input: false },
      { kind: 'gap', reason: 'the workstation is locked' },
      { kind: 'resumed' },
      { kind: 'ended', reason: 'the endpoint refused the session' },
    ];
    for (const meta of metas) {
      const decoded = decodeFrame(frame(meta));
      expect(decoded.meta).toEqual(meta);
      expect(decoded.payload.length).toBe(0);
    }
  });

  it('carries a null geometry on started rather than inventing one', () => {
    // The agent never reports screen size on accept. A viewer that treated
    // this as a number would size its canvas 0x0 and paint nothing.
    const { meta } = decodeFrame(
      frame({ kind: 'started', session_id: 's', screen_w: null, screen_h: null, allow_input: false }),
    );
    expect(meta.kind).toBe('started');
    if (meta.kind === 'started') {
      expect(meta.screen_w).toBeNull();
      expect(meta.screen_h).toBeNull();
    }
  });

  it('rejects a message it cannot read instead of painting from it', () => {
    // Shorter than the prefix itself.
    expect(() => decodeFrame(new Uint8Array([1, 2]).buffer)).toThrow(FrameDecodeError);

    // Prefix longer than the message: a truncated frame must not slice past
    // the end and hand back a half-parsed meta.
    const truncated = new Uint8Array(8);
    new DataView(truncated.buffer).setUint32(0, 999, true);
    expect(() => decodeFrame(truncated.buffer)).toThrow(FrameDecodeError);

    // Meta that is not JSON at all.
    const notJson = new Uint8Array(4 + 3);
    new DataView(notJson.buffer).setUint32(0, 3, true);
    notJson.set(new TextEncoder().encode('{{{'), 4);
    expect(() => decodeFrame(notJson.buffer)).toThrow(FrameDecodeError);

    // Valid JSON with no `kind` — the field every branch dispatches on.
    expect(() => decodeFrame(frame({ frame_seq: 1 }))).toThrow(FrameDecodeError);
  });

  it('handles a zero-length payload and a zero-length meta distinctly', () => {
    expect(decodeFrame(frame({ kind: 'resumed' })).payload.length).toBe(0);
    const empty = new Uint8Array(4);
    // meta_len 0 → no meta to parse, which is not a frame we understand.
    expect(() => decodeFrame(empty.buffer)).toThrow(FrameDecodeError);
  });
});

describe('handshake', () => {
  it('offers the protocol first and the credential second', () => {
    expect(subprotocols('jwt.abc.def')).toEqual([SUBPROTOCOL, 'bearer.jwt.abc.def']);
  });

  it('builds a same-origin socket url, upgrading scheme with the page', () => {
    expect(remoteSocketUrl('MINIPC', { protocol: 'http:', host: '127.0.0.1:8080' } as Location)).toBe(
      'ws://127.0.0.1:8080/api/remote/MINIPC/ws',
    );
    expect(remoteSocketUrl('MINIPC', { protocol: 'https:', host: 'kanade.example' } as Location)).toBe(
      'wss://kanade.example/api/remote/MINIPC/ws',
    );
  });

  it('escapes a pc_id rather than letting it shape the path', () => {
    // pc_ids are OS hostnames taken verbatim and are never case-folded, so
    // the encoder must not touch case — but it must stop a stray slash from
    // silently addressing a different route.
    expect(remoteSocketUrl('pc/../evil', { protocol: 'http:', host: 'h' } as Location)).toBe(
      'ws://h/api/remote/pc%2F..%2Fevil/ws',
    );
    expect(remoteSocketUrl('MiXeD', { protocol: 'http:', host: 'h' } as Location)).toBe(
      'ws://h/api/remote/MiXeD/ws',
    );
  });
});
