import { ArrowLeft, Loader2, MonitorOff, MonitorPlay, Play, Square } from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useParams } from 'react-router-dom';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { useAuth } from '@/lib/auth';
import {
  decodeFrame,
  remoteSocketUrl,
  subprotocols,
  SUBPROTOCOL,
  type TileMeta,
} from '@/lib/remoteFrame';

/** Where the stream is, from the viewer's point of view. */
type Phase =
  | { s: 'idle' }
  | { s: 'connecting' }
  | { s: 'live' }
  /** Capture stopped but the session is alive — the screen is unavailable,
   *  which is a different thing from "nothing is changing". */
  | { s: 'unavailable'; reason: string }
  | { s: 'ended'; reason: string };

export function RemoteScreen() {
  const { t } = useTranslation('remote');
  const { pcId = '' } = useParams();
  const { hasRole } = useAuth();
  const canView = hasRole('operator');

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const socketRef = useRef<WebSocket | null>(null);
  const [phase, setPhase] = useState<Phase>({ s: 'idle' });
  const [screen, setScreen] = useState<{ w: number; h: number } | null>(null);
  const [tiles, setTiles] = useState(0);
  // Tile draws are serialised through this chain, and the message handler is
  // deliberately NOT async. An async handler returns at its first await, so
  // the browser starts the next message immediately and two tiles decode
  // concurrently — painting in whatever order their JPEGs happen to finish.
  // Tiles within one frame are disjoint, but consecutive frames touch the same
  // rectangles, so a late-resolving older tile can overwrite a newer one. That
  // heals on the next change to that region and never heals if the desktop
  // then goes idle. Chaining makes paint order equal arrival order, which is
  // the order the wire already guarantees.
  const drawChain = useRef<Promise<void>>(Promise.resolve());

  const disconnect = useCallback(() => {
    socketRef.current?.close();
    socketRef.current = null;
  }, []);

  const connect = useCallback(() => {
    const token = localStorage.getItem('kanade_token') ?? '';
    setPhase({ s: 'connecting' });
    setScreen(null);
    setTiles(0);
    drawChain.current = Promise.resolve();

    const ws = new WebSocket(remoteSocketUrl(pcId), subprotocols(token));
    // Frames arrive as binary; without this every message would surface as a
    // Blob and each tile would cost an extra async hop before it can be
    // decoded.
    ws.binaryType = 'arraybuffer';
    socketRef.current = ws;

    ws.onopen = () => {
      // The server must select our protocol and never echo the credential
      // entry. Anything else is not a backend we should stream a desktop to.
      if (ws.protocol !== SUBPROTOCOL) {
        setPhase({ s: 'ended', reason: t('errors.badProtocol', { got: ws.protocol || '(none)' }) });
        ws.close();
      }
    };

    // Decode and paint one tile. Only ever called from `drawChain`, so calls
    // are serialised; the `socketRef` checks drop work belonging to a session
    // the operator already left, keeping a previous machine's pixels off the
    // new canvas.
    const paintTile = async (meta: TileMeta, payload: Uint8Array<ArrayBuffer>) => {
      if (socketRef.current !== ws) return;
      const canvas = canvasRef.current;
      if (!canvas) return;
      // Every tile repeats the desktop size, which is what makes joining
      // mid-session work — size from the first one that arrives.
      if (canvas.width !== meta.screen_w || canvas.height !== meta.screen_h) {
        canvas.width = meta.screen_w;
        canvas.height = meta.screen_h;
        setScreen({ w: meta.screen_w, h: meta.screen_h });
      }
      const bitmap = await createImageBitmap(new Blob([payload], { type: `image/${meta.encoding}` }));
      if (socketRef.current === ws && canvasRef.current) {
        canvasRef.current.getContext('2d')?.drawImage(bitmap, meta.x, meta.y);
        setTiles((n) => n + 1);
        setPhase((p) => (p.s === 'live' ? p : { s: 'live' }));
      }
      bitmap.close();
    };

    ws.onmessage = (ev: MessageEvent<ArrayBuffer>) => {
      let frame;
      try {
        frame = decodeFrame(ev.data);
      } catch (e) {
        // One unreadable message costs a stale rectangle; tearing the session
        // down over it would cost the whole session. Same reasoning the
        // backend applies to a malformed NATS message.
        console.warn('[remote] undecodable frame', e);
        return;
      }

      const { meta, payload } = frame;
      switch (meta.kind) {
        case 'started':
          // Deliberately NOT sizing the canvas from meta.screen_w/h: the
          // agent answers Start before its capture child has taken a frame,
          // so that geometry is null in practice. The first tile carries it.
          setPhase({ s: 'live' });
          break;

        case 'tile':
          // `.catch` keeps one undecodable JPEG from leaving the chain in a
          // rejected state, which would silently stop every later tile.
          drawChain.current = drawChain.current
            .then(() => paintTile(meta, payload))
            .catch((e) => console.warn('[remote] tile draw failed', e));
          break;

        case 'gap':
          setPhase({ s: 'unavailable', reason: meta.reason });
          break;

        case 'resumed':
          // Needed because a recovered desktop nobody is touching produces no
          // tiles at all — without this the operator would keep reading
          // "unavailable" long after capture came back.
          setPhase({ s: 'live' });
          break;

        case 'ended':
          setPhase({ s: 'ended', reason: meta.reason });
          break;
      }
    };

    ws.onerror = () => {
      // The handshake failure reason (401/403) is not exposed to script by
      // design, so say what we can rather than inventing a cause.
      setPhase((p) => (p.s === 'ended' ? p : { s: 'ended', reason: t('errors.socket') }));
    };

    ws.onclose = () => {
      socketRef.current = null;
      setPhase((p) => (p.s === 'ended' ? p : { s: 'ended', reason: t('errors.closed') }));
    };
  }, [pcId, t]);

  // Closing the socket is what stops the capture child on the endpoint, so it
  // must happen on unmount too — navigating away otherwise leaves a machine
  // encoding its screen for nobody.
  useEffect(() => () => disconnect(), [disconnect]);

  if (!canView) {
    return (
      <Card>
        <CardContent className="p-6 text-sm text-muted-foreground">{t('forbidden')}</CardContent>
      </Card>
    );
  }

  const streaming = phase.s === 'connecting' || phase.s === 'live' || phase.s === 'unavailable';

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to={`/agents/${encodeURIComponent(pcId)}`}>
            <ArrowLeft className="size-4" />
            {t('back')}
          </Link>
        </Button>
        <h2 className="text-xl">
          <code className="text-base">{pcId}</code>
        </h2>
        <StatusBadge phase={phase} t={t} />
        {screen && (
          <span className="text-xs text-muted-foreground">
            {screen.w}×{screen.h} · {t('tiles', { count: tiles })}
          </span>
        )}
        <div className="ml-auto">
          {streaming ? (
            <Button variant="secondary" size="sm" onClick={disconnect}>
              <Square className="size-4" />
              {t('stop')}
            </Button>
          ) : (
            <Button size="sm" onClick={connect}>
              <Play className="size-4" />
              {t('start')}
            </Button>
          )}
        </div>
      </div>

      <Card>
        <CardContent className="p-2">
          <div className="relative bg-black/90 rounded">
            {/* Sized in desktop pixels by the first tile, scaled to the
                viewport with CSS. Coordinates stay desktop-space, which is
                what PR5's input injection needs — a canvas scaled by
                attribute would make correctness depend on window size. */}
            <canvas ref={canvasRef} className="w-full h-auto block rounded" />
            {phase.s !== 'live' && (
              <div className="absolute inset-0 grid place-items-center bg-background/70 text-sm">
                <Overlay phase={phase} t={t} />
              </div>
            )}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

function StatusBadge({ phase, t }: { phase: Phase; t: (k: string) => string }) {
  switch (phase.s) {
    case 'live':
      return <Badge variant="success">{t('status.live')}</Badge>;
    case 'connecting':
      return <Badge variant="amber">{t('status.connecting')}</Badge>;
    case 'unavailable':
      return <Badge variant="amber">{t('status.unavailable')}</Badge>;
    case 'ended':
      return <Badge>{t('status.ended')}</Badge>;
    default:
      return <Badge>{t('status.idle')}</Badge>;
  }
}

function Overlay({ phase, t }: { phase: Phase; t: (k: string) => string }) {
  switch (phase.s) {
    case 'idle':
      return (
        <span className="flex items-center gap-2 text-muted-foreground">
          <MonitorPlay className="size-4" />
          {t('overlay.idle')}
        </span>
      );
    case 'connecting':
      return (
        <span className="flex items-center gap-2 text-muted-foreground">
          <Loader2 className="size-4 animate-spin" />
          {t('overlay.connecting')}
        </span>
      );
    case 'unavailable':
      return (
        <span className="flex flex-col items-center gap-1 text-center">
          <span className="flex items-center gap-2">
            <MonitorOff className="size-4" />
            {t('overlay.unavailable')}
          </span>
          <span className="text-xs text-muted-foreground">{phase.reason}</span>
        </span>
      );
    case 'ended':
      return (
        <span className="flex flex-col items-center gap-1 text-center">
          <span>{t('overlay.ended')}</span>
          <span className="text-xs text-muted-foreground">{phase.reason}</span>
        </span>
      );
    default:
      return null;
  }
}
