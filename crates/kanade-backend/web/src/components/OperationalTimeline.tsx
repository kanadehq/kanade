import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';

import { fmtIsoLocal } from '@/lib/utils';

// One raw operational event. Mirrors the backend `OpEvent`
// (api/analytics.rs) and the per-PC rows the Events page already holds.
export type OpEvent = { at: string; kind: string };

// A reconstructed lane interval. `openStart` / `openEnd` mark a span that
// began before the window or is still open at its end (the matching
// start/end event falls outside [t0, t1]), so the strip can hint that the
// state continues beyond what's shown.
type Span = { from: number; to: number; openStart: boolean; openEnd: boolean };

// The fixed operational lanes. Each is reconstructed from a START kind that
// opens the interval and an END kind that closes it:
//   power   — agent/host is up (boot / service-start → shutdown / service-stop)
//   session — a user is signed in (logon → logoff)
//   sleep   — the host is suspended (sleep → resume)
//   active  — an interactive user is actually using the box (active → idle);
//             a filled span = present, a gap = idle. Fed by the agent-native
//             idle sampler (#841), the one signal not in the Event Log.
// The kinds line up with the backend `op_timeline` query's IN-list and the
// emitters (collect-winlog-events for the log-sourced lanes, the idle
// sampler for active/idle).
const OP_LANES = [
  {
    key: 'power',
    starts: ['boot', 'log_service_started'],
    ends: ['shutdown', 'unexpected_shutdown', 'log_service_stopped'],
    color: '#10b981', // emerald-500
  },
  {
    key: 'session',
    starts: ['logon'],
    ends: ['logoff'],
    color: '#8b5cf6', // violet-500
  },
  {
    key: 'sleep',
    starts: ['sleep'],
    ends: ['resume'],
    color: '#f59e0b', // amber-500
  },
  {
    key: 'active',
    starts: ['active'],
    ends: ['idle'],
    color: '#0ea5e9', // sky-500
  },
] as const;

type LaneKey = (typeof OP_LANES)[number]['key'];

// Every kind any lane reads — the Events page filters its rows down to these
// before handing them to the strip, and it matches the backend
// `op_timeline` query's IN-list.
export const OP_TIMELINE_KINDS: readonly string[] = OP_LANES.flatMap((l) => [
  ...l.starts,
  ...l.ends,
]);

// Walk the lane's start/end events in ascending time and pair them into
// spans, clamped to [t0, t1]. An end with no open start began before the
// window (`openStart`); a start still open at the end of the stream runs to
// the window edge — but no further than `now`, so a window that extends into
// the future (e.g. "today" before midnight) doesn't paint an ongoing state
// across hours that haven't happened yet (`openEnd`). A second start while
// already open is ignored (a missed end shouldn't fragment the interval).
function buildSpans(
  events: OpEvent[],
  starts: readonly string[],
  ends: readonly string[],
  t0: number,
  t1: number,
  now: number,
): Span[] {
  const startSet = new Set(starts);
  const endSet = new Set(ends);
  const sorted = events
    .map((e) => ({ ts: Date.parse(e.at), kind: e.kind }))
    .filter((e) => !Number.isNaN(e.ts) && (startSet.has(e.kind) || endSet.has(e.kind)))
    .sort((a, b) => a.ts - b.ts);

  const spans: Span[] = [];
  let open: number | null = null;
  // A "carry-in" span covers state that was already open before the stream
  // began (an end with no matching start). It's valid only before the first
  // start/interval — set once we've seen a start OR synthesized it, so a run
  // of consecutive ends (e.g. the power lane's log_service_stopped + shutdown)
  // can't each push another t0-anchored span and overlap.
  let carriedIn = false;
  for (const e of sorted) {
    if (startSet.has(e.kind)) {
      carriedIn = true;
      if (open === null) open = e.ts;
      // already open → ignore the duplicate start (missed end).
    } else if (open !== null) {
      // `openStart` when the start was seeded from before the window.
      spans.push({ from: open, to: e.ts, openStart: open < t0, openEnd: false });
      open = null;
    } else if (!carriedIn) {
      spans.push({ from: t0, to: e.ts, openStart: true, openEnd: false });
      carriedIn = true;
    }
  }
  if (open !== null) {
    // The live edge: an open interval is only known up to `now`, never past
    // it. Clamp to min(t1, now) so a future-extending window stops the strip
    // at the present instead of the selected day's end. The square `openEnd`
    // edge still reads as "ongoing".
    spans.push({ from: open, to: Math.min(t1, now), openStart: open < t0, openEnd: true });
  }
  // Clamp to the window and drop anything that collapses to zero width.
  return spans
    .map((s) => ({ ...s, from: Math.max(s.from, t0), to: Math.min(s.to, t1) }))
    .filter((s) => s.to > s.from);
}

// Point markers for the lane's events (instantaneous boot/logon/… ticks),
// so a lone event with no pair still shows even when it forms no span.
function laneMarkers(
  events: OpEvent[],
  starts: readonly string[],
  ends: readonly string[],
  t0: number,
  t1: number,
): { ts: number; kind: string }[] {
  const keep = new Set([...starts, ...ends]);
  return events
    .map((e) => ({ ts: Date.parse(e.at), kind: e.kind }))
    .filter((e) => !Number.isNaN(e.ts) && keep.has(e.kind) && e.ts >= t0 && e.ts <= t1);
}

// ~5 evenly spaced axis ticks across [t0, t1]. Short HH:mm when the window
// fits in a day, MM/DD HH:mm otherwise (same rule as the Events scatter).
function axisTicks(t0: number, t1: number): { ts: number; label: string }[] {
  const span = t1 - t0;
  if (!(span > 0)) return [];
  const multiDay = span > 24 * 60 * 60 * 1000;
  const fmt = (ts: number) => {
    const d = new Date(ts);
    const hh = d.getHours().toString().padStart(2, '0');
    const mm = d.getMinutes().toString().padStart(2, '0');
    return multiDay ? `${d.getMonth() + 1}/${d.getDate()} ${hh}:${mm}` : `${hh}:${mm}`;
  };
  const N = 4;
  return Array.from({ length: N + 1 }, (_, i) => {
    const ts = t0 + (span * i) / N;
    return { ts, label: fmt(ts) };
  });
}

/**
 * Per-PC operational swimlane. Folds a window of raw operational events
 * (power / session / sleep kinds) into lane spans the operator can read at a
 * glance: when the host was up, who was signed in, when it slept. Shared by
 * the Analytics `op_timeline` widget (which receives a server-computed
 * window) and the Events page strip (which derives the window from the
 * rendered events). The active/idle lane is fed by the agent-native idle
 * sampler (#841); a filled span = active, a gap = idle.
 *
 * `from` / `to` bound the window; when omitted they fall back to the
 * earliest / latest event so the Events page can use it without a window.
 */
export function OperationalTimeline({
  events,
  from,
  to,
}: {
  events: OpEvent[];
  from?: string;
  to?: string;
}) {
  const { t } = useTranslation('events');

  const [t0, t1] = useMemo(() => {
    const tsList = events.map((e) => Date.parse(e.at)).filter((n) => !Number.isNaN(n));
    // reduce (not Math.min(...tsList)): the spread would blow the call stack
    // on a very large event set, and the backend op_timeline query is
    // unbounded. Empty list folds to ±Infinity → caught by the guard below.
    const lo = from ? Date.parse(from) : tsList.reduce((min, v) => Math.min(min, v), Infinity);
    const hi = to ? Date.parse(to) : tsList.reduce((max, v) => Math.max(max, v), -Infinity);
    if (!Number.isFinite(lo) || !Number.isFinite(hi)) {
      // No usable window (no events and no bounds) — caller renders the
      // empty note; return a degenerate range so hooks below stay stable.
      return [0, 0];
    }
    if (!from && !to && hi === lo) {
      // A single event with no explicit bounds: pad ±1min so the strip still
      // renders its marker instead of collapsing to the empty state.
      const pad = 60_000;
      return [lo - pad, hi + pad];
    }
    if (hi <= lo) return [0, 0];
    return [lo, hi];
  }, [events, from, to]);

  const lanes = useMemo(() => {
    if (t1 <= t0) return [];
    // Snapshot "now" once per render so open intervals don't paint past the
    // present when the window runs into the future (today before midnight).
    const now = Date.now();
    return OP_LANES.map((lane) => ({
      key: lane.key as LaneKey,
      color: lane.color,
      spans: buildSpans(events, lane.starts, lane.ends, t0, t1, now),
      markers: laneMarkers(events, lane.starts, lane.ends, t0, t1),
    }));
  }, [events, t0, t1]);

  const ticks = useMemo(() => axisTicks(t0, t1), [t0, t1]);

  if (t1 <= t0) {
    return <div className="text-muted text-sm">{t('opTimeline.empty')}</div>;
  }

  const span = t1 - t0;
  const pct = (ts: number) => ((ts - t0) / span) * 100;

  return (
    <div className="space-y-1.5">
      {lanes.map((lane) => (
        <div key={lane.key} className="flex items-center gap-2">
          <div className="flex w-28 shrink-0 items-center gap-1.5 text-xs text-muted">
            <span
              className="inline-block size-2 shrink-0 rounded-sm"
              style={{ backgroundColor: lane.color }}
            />
            <span className="truncate">{t(`opTimeline.lanes.${lane.key}`)}</span>
          </div>
          <div className="relative h-6 flex-1 overflow-hidden rounded-sm bg-muted/10">
            {lane.spans.map((s, i) => {
              const note = [
                s.openStart ? t('opTimeline.openStart') : '',
                s.openEnd ? t('opTimeline.openEnd') : '',
              ]
                .filter(Boolean)
                .join(' · ');
              const title =
                t('opTimeline.spanTooltip', {
                  lane: t(`opTimeline.lanes.${lane.key}`),
                  from: fmtIsoLocal(new Date(s.from).toISOString()),
                  to: fmtIsoLocal(new Date(s.to).toISOString()),
                }) + (note ? ` (${note})` : '');
              return (
                <div
                  key={i}
                  className="absolute top-0 h-full"
                  style={{
                    left: `${pct(s.from)}%`,
                    width: `${Math.max(pct(s.to) - pct(s.from), 0.4)}%`,
                    backgroundColor: lane.color,
                    opacity: 0.65,
                    // Soften the edge that continues beyond the window so an
                    // open span reads as "ongoing", not a hard boundary.
                    borderTopLeftRadius: s.openStart ? 0 : 2,
                    borderBottomLeftRadius: s.openStart ? 0 : 2,
                    borderTopRightRadius: s.openEnd ? 0 : 2,
                    borderBottomRightRadius: s.openEnd ? 0 : 2,
                  }}
                  title={title}
                />
              );
            })}
            {lane.markers.map((m, i) => (
              <div
                key={`m-${i}`}
                className="absolute top-0 h-full w-px"
                style={{ left: `${pct(m.ts)}%`, backgroundColor: lane.color }}
                title={t('opTimeline.markerTooltip', {
                  kind: m.kind,
                  at: fmtIsoLocal(new Date(m.ts).toISOString()),
                })}
              />
            ))}
          </div>
        </div>
      ))}

      {/* Time axis. */}
      <div className="relative ml-[120px] h-4 text-[9px] text-muted">
        {ticks.map((tick, i) => (
          <span
            key={i}
            className="absolute -translate-x-1/2 whitespace-nowrap"
            style={{
              left: `${pct(tick.ts)}%`,
              transform:
                i === 0
                  ? 'translateX(0)'
                  : i === ticks.length - 1
                    ? 'translateX(-100%)'
                    : 'translateX(-50%)',
            }}
          >
            {tick.label}
          </span>
        ))}
      </div>
    </div>
  );
}
