/**
 * Decoder for the remote-assistance socket (#1140 PR3c-2).
 *
 * Every message on `GET /api/remote/{pc_id}/ws` is binary and shaped:
 *
 *     [u32 LE meta_len][meta JSON][payload]
 *
 * The length prefix covers **only the metadata**. The payload is whatever
 * remains, so there are never two numbers that could disagree — see
 * `crates/kanade-backend/src/api/remote.rs`.
 *
 * Kept as a pure function over an `ArrayBuffer` because the byte offsets are
 * the one part of the viewer that can be wrong without looking wrong: an
 * off-by-four paints a JPEG missing its header, which fails as "the image
 * won't decode" three layers from the cause.
 */

/** Tile geometry, mirrored from `kanade_shared::wire::remote::FrameMeta`. */
export interface TileMeta {
  kind: 'tile';
  frame_seq: number;
  tile_index: number;
  tile_count: number;
  x: number;
  y: number;
  w: number;
  h: number;
  /** Full desktop size, repeated on every tile — this is what a viewer sizes
   *  its canvas from. See `StartedMeta` for why not that. */
  screen_w: number;
  screen_h: number;
  captured_at_ms: number;
  encoding: 'jpeg' | 'webp';
}

/**
 * The stream is live.
 *
 * `screen_w` / `screen_h` are **normally absent**: the agent answers `Start`
 * before its capture child has taken a frame, deliberately, so that accepting
 * is not blocked on a display round-trip. A viewer therefore sizes its canvas
 * from the first tile and must never wait for the geometry here — verified on
 * real hardware, where this always arrives null.
 */
export interface StartedMeta {
  kind: 'started';
  session_id: string;
  screen_w: number | null;
  screen_h: number | null;
  allow_input: boolean;
}

/** Capture stopped — locked workstation, UAC prompt, display mode change. */
export interface GapMeta {
  kind: 'gap';
  reason: string;
}

/** Capture works again. Distinct from a tile because a recovered desktop that
 *  nobody is touching produces no tiles at all. */
export interface ResumedMeta {
  kind: 'resumed';
}

/** The stream is over, and why. Always the last message on the socket. */
export interface EndedMeta {
  kind: 'ended';
  reason: string;
}

export type FrameMeta = TileMeta | StartedMeta | GapMeta | ResumedMeta | EndedMeta;

export interface RemoteFrame {
  meta: FrameMeta;
  /** Encoded image bytes for a tile; empty for every other kind.
   *  Typed over `ArrayBuffer` (not `ArrayBufferLike`) so it drops straight
   *  into a `Blob` — the DOM's `BlobPart` excludes `SharedArrayBuffer`. */
  payload: Uint8Array<ArrayBuffer>;
}

export class FrameDecodeError extends Error {}

/**
 * Split one socket message into its metadata and payload.
 *
 * Throws [`FrameDecodeError`] rather than returning a partial frame: a
 * message this function cannot read is not a degraded tile, it is a message
 * whose shape we do not understand, and painting from it would put garbage on
 * an operator's screen.
 */
export function decodeFrame(buf: ArrayBuffer): RemoteFrame {
  if (buf.byteLength < 4) {
    throw new FrameDecodeError(`frame is ${buf.byteLength} bytes, too short for a length prefix`);
  }
  const view = new DataView(buf);
  const metaLen = view.getUint32(0, /* littleEndian */ true);
  if (4 + metaLen > buf.byteLength) {
    throw new FrameDecodeError(`meta claims ${metaLen} bytes but only ${buf.byteLength - 4} remain`);
  }

  const json = new TextDecoder().decode(new Uint8Array(buf, 4, metaLen));
  let meta: FrameMeta;
  try {
    meta = JSON.parse(json) as FrameMeta;
  } catch (e) {
    throw new FrameDecodeError(`meta is not JSON: ${String(e)}`);
  }
  if (typeof (meta as { kind?: unknown })?.kind !== 'string') {
    throw new FrameDecodeError('meta has no kind');
  }

  return { meta, payload: new Uint8Array(buf, 4 + metaLen) };
}

/**
 * Subprotocols to offer when opening the socket.
 *
 * The credential rides here because a browser cannot set an `Authorization`
 * header on a WebSocket, and `?token=` would leave it in history, proxy logs
 * and `Referer`. The server echoes back only `kanade.remote.v1` — never the
 * credential entry — so a handshake that comes back naming anything else is a
 * server we should not be talking to.
 */
export const SUBPROTOCOL = 'kanade.remote.v1';

export function subprotocols(token: string): string[] {
  return [SUBPROTOCOL, `bearer.${token}`];
}

/** `ws(s)://…/api/remote/<pc_id>/ws`, matching the page's own origin so the
 *  socket follows the SPA through whatever host/port it is served from. */
export function remoteSocketUrl(pcId: string, loc: Location = window.location): string {
  const scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${loc.host}/api/remote/${encodeURIComponent(pcId)}/ws`;
}
