import { type MouseEvent as ReactMouseEvent, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { AGENT_ACTIVE_THRESHOLD_MS, fmtIsoLocal } from '@/lib/utils';

// One raw operational event. Mirrors the backend `OpEvent`
// (api/analytics.rs) and the per-PC rows the Events page already holds.
export type OpEvent = { at: string; kind: string };

// A reconstructed lane interval. `openStart` / `openEnd` mark a span that
// began before the window or is still open at its end (the matching
// start/end event falls outside [t0, t1]), so the strip can hint that the
// state continues beyond what's shown. `uncertain` marks the tail past the
// agent's last heartbeat — the state is asserted but unconfirmed (the agent
// is offline), so the strip hatches it instead of painting it solid.
type Span = {
  from: number;
  to: number;
  openStart: boolean;
  openEnd: boolean;
  uncertain?: boolean;
  // The confirmed head of a heartbeat split — it abuts the hatched uncertain
  // tail, so its right edge must be square (not the usual rounded 2px) to read
  // as continuous with the tail. Kept separate from `openEnd` so the tooltip's
  // "still open at window end" note stays off (the head is a hard cut, not an
  // ongoing state running off the window edge).
  cutByHeartbeat?: boolean;
};

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

// Coalesce overlapping / touching spans into one. Used to build the presence
// envelope (active ∪ idle spans) for the winlog-less backfill: unioning the
// two directions keeps buildSpans' carry-in at *both* edges (a leading `idle`
// implies the host was active right up to it; a leading `active` implies it was
// idle-but-on before), so the envelope covers the window edge instead of
// starting at the first raw event and leaving a sliver where the active lane's
// own carry-in already paints. Assumes `spans` are within [t0, t1]. The merged
// span keeps the earliest contributor's `openStart` and, when a later span
// extends the end, that span's `openEnd`.
function mergeSpans(spans: Span[]): Span[] {
  const sorted = [...spans].sort((a, b) => a.from - b.from);
  const out: Span[] = [];
  for (const s of sorted) {
    const last = out[out.length - 1];
    if (last && s.from <= last.to) {
      // Coincident boundaries must OR the "continues beyond" flags, not drop
      // them: two spans sharing `from` (both carried in to t0) keep openStart
      // if either has it, and two sharing the end keep openEnd if either is
      // open — otherwise the flag of a same-boundary sibling sorted first wins
      // and silently clears it.
      if (s.from === last.from) last.openStart = last.openStart || s.openStart;
      if (s.to > last.to) {
        last.to = s.to;
        last.openEnd = s.openEnd;
      } else if (s.to === last.to) {
        last.openEnd = last.openEnd || s.openEnd;
      }
    } else {
      out.push({ ...s });
    }
  }
  return out;
}

// Intersect a subordinate lane's spans with the power lane's ON intervals.
// The active / session / sleep lanes are reconstructed independently of
// power, but a host can't be in any of those states while it's switched
// off — and their state often isn't closed cleanly across a power cycle:
// the idle sampler emits no `idle` on shutdown (an unexpected power loss
// emits nothing at all), so a stale `active` seeded from before the window
// would otherwise paint straight across a powered-off gap (#841 follow-up).
// Clipping to the power ON spans drops those phantom segments. A boundary
// that the clip introduces (mid-span, where power toggled) is a hard edge,
// so it clears the `openStart` / `openEnd` "continues beyond" hint; an
// original window edge keeps it.
function clipToOn(spans: Span[], on: { from: number; to: number }[]): Span[] {
  const out: Span[] = [];
  for (const s of spans) {
    for (const iv of on) {
      const from = Math.max(s.from, iv.from);
      const to = Math.min(s.to, iv.to);
      if (to > from) {
        out.push({
          from,
          to,
          openStart: s.openStart && from === s.from,
          openEnd: s.openEnd && to === s.to,
        });
      }
    }
  }
  return out;
}

// Split a lane's spans at the "last confirmed" boundary. Everything a lane
// asserts *after* the agent's last heartbeat is unconfirmed: an unexpected
// power loss emits no shutdown and the idle sampler stops, so the open span
// would otherwise paint an assumed-ON strip straight to `now` even though the
// host may be dark. `certainEdge` is the newest instant we still trust —
// min(liveEdge, lastHeartbeat + grace). An open (ongoing) span that reaches
// past it is cut there: the head stays solid, the tail becomes an `uncertain`
// span the strip hatches. Closed historical spans (a real start/end pair) are
// left untouched — they're confirmed regardless of the current heartbeat. It
// self-corrects on reconnect: a fresh heartbeat pushes certainEdge to `now`
// (un-hatching), and backfilled winlog closes/reopens the power span so a
// real powered-off gap ends up unpainted instead of hatched.
function gateToHeartbeat(spans: Span[], certainEdge: number, liveEdge: number): Span[] {
  // certainEdge >= liveEdge → agent online, or no heartbeat info (the Events
  // page): nothing to gate, keep the solid strips exactly as before.
  if (certainEdge >= liveEdge) return spans;
  const out: Span[] = [];
  for (const s of spans) {
    if (!s.openEnd || s.to <= certainEdge) {
      out.push(s); // closed span, or one already ending inside the trusted region
    } else if (s.from >= certainEdge) {
      out.push({ ...s, uncertain: true }); // wholly in the unconfirmed tail
    } else {
      // Straddles the boundary: solid head, hatched tail.
      out.push({
        from: s.from,
        to: certainEdge,
        openStart: s.openStart,
        openEnd: false,
        cutByHeartbeat: true,
      });
      out.push({ from: certainEdge, to: s.to, openStart: false, openEnd: true, uncertain: true });
    }
  }
  return out;
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

// Geometry of the lane-label column, in px. Four things have to agree on
// where the tracks start: the label column's own width, the row gap after it,
// the axis's left margin, and the readout's `left`. They used to agree by
// arithmetic coincidence — `w-28` + `gap-2` on one side and a literal
// `ml-[120px]` on the other — which is both easy to desync (a longer
// translated lane name needing a wider column silently breaks the pointer
// mapping, with no type error and no failing test) and not actually
// equivalent: Tailwind's spacing scale is rem-based, so the two only match at
// a 16px root font size. A user with a larger root font — an ordinary
// accessibility setting — would scale the column while the literal stayed
// pinned, throwing the crosshair off the spans it's pointing at. Driving all
// four from these constants keeps them in px and in step.
const LANE_LABEL_WIDTH_PX = 112;
const LANE_LABEL_GAP_PX = 8;
const TRACK_OFFSET_PX = LANE_LABEL_WIDTH_PX + LANE_LABEL_GAP_PX;

const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;

// Candidate tick intervals, coarsest-last. Every entry divides evenly into a
// day (or is a whole number of days) so ticks land on times an operator
// already thinks in — 0:00 / 6:00 / 12:00 / 18:00, never 10:38.
const TICK_STEPS_MS = [
  5 * MINUTE_MS,
  15 * MINUTE_MS,
  30 * MINUTE_MS,
  HOUR_MS,
  2 * HOUR_MS,
  3 * HOUR_MS,
  6 * HOUR_MS,
  12 * HOUR_MS,
  DAY_MS,
  2 * DAY_MS,
  7 * DAY_MS,
  14 * DAY_MS,
  28 * DAY_MS,
];

// Upper bound on tick intervals; the strip is narrow, so more than this and
// the labels collide.
const MAX_TICK_INTERVALS = 6;

/** Local midnight of the day containing `ts`. */
function localDayStart(ts: number): number {
  const d = new Date(ts);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/**
 * Local midnight of the day after the one containing `ts`. Goes through
 * `setHours` on a +25h probe rather than adding 86_400_000, so a DST
 * transition can't drift the anchor by an hour.
 */
function nextLocalDayStart(ts: number): number {
  return localDayStart(localDayStart(ts) + 25 * HOUR_MS);
}

/**
 * Coarsest interval that keeps `span` under `maxIntervals` divisions.
 *
 * Past the end of the table (a span over ~168 days, reachable from the
 * Analytics widget, whose `from` has no floor) the table can't satisfy the
 * bound, so the step is computed instead — rounded up to whole days, which
 * keeps it midnight-alignable. Falling back to the largest table entry would
 * silently ignore `maxIntervals` and crowd the strip with hundreds of ticks.
 */
function pickStep(span: number, maxIntervals: number): number {
  const s = TICK_STEPS_MS.find((x) => span / x <= maxIntervals);
  if (s !== undefined) return s;
  return Math.ceil(span / maxIntervals / DAY_MS) * DAY_MS;
}

/**
 * Round local times at `step`, anchored to local midnight — not to the epoch,
 * which in a non-UTC zone would put a 6h step on 3:00 / 9:00 / 15:00 / 21:00.
 */
function roundTimes(t0: number, t1: number, step: number): number[] {
  const times: number[] = [];
  if (step >= DAY_MS) {
    // Whole-day steps: walk local midnights and keep every k-th, so the ticks
    // stay on date boundaries instead of drifting into mid-day.
    const k = Math.max(1, Math.round(step / DAY_MS));
    let d = localDayStart(t0);
    if (d < t0) d = nextLocalDayStart(d);
    let i = 0;
    while (d <= t1 && times.length < 64) {
      times.push(d);
      for (let j = 0; j < k && d <= t1; j++) d = nextLocalDayStart(d);
      i++;
      if (i > 400) break;
    }
  } else {
    // Sub-day steps: re-anchor at each local midnight so the pattern restarts
    // cleanly every day (and a DST day can't shift every later tick).
    let day = localDayStart(t0);
    let guard = 0;
    while (day <= t1 && times.length < 64 && guard++ < 400) {
      const next = nextLocalDayStart(day);
      for (let ts = day; ts < next && ts <= t1; ts += step) {
        if (ts >= t0) times.push(ts);
      }
      day = next;
    }
  }
  return times;
}

/**
 * Axis ticks on round local times. Ticks that land on midnight are labelled
 * with the date, the rest with HH:mm, so the day a time belongs to is always
 * readable from the nearest date tick to its left.
 */
export function axisTicks(t0: number, t1: number): { ts: number; label: string; isDay: boolean }[] {
  const span = t1 - t0;
  if (!(span > 0)) return [];
  return roundTimes(t0, t1, pickStep(span, MAX_TICK_INTERVALS)).map((ts) => {
    const d = new Date(ts);
    const isDay = ts === localDayStart(ts);
    const hh = d.getHours().toString().padStart(2, '0');
    const mm = d.getMinutes().toString().padStart(2, '0');
    return {
      ts,
      label: isDay ? `${d.getMonth() + 1}/${d.getDate()}` : `${hh}:${mm}`,
      isDay,
    };
  });
}

/**
 * Gridlines for the lane tracks. Deliberately finer than the labelled ticks:
 * labels have to stay sparse enough not to collide, but the lanes have room
 * for more structure than that, and reading a span's extent off ~5 lines
 * across two days means eyeballing halves and thirds. Allowing twice as many
 * intervals picks the next step down from the same table, so the lines stay
 * on round times and every label still sits on one. Day boundaries are
 * flagged so the renderer can draw them stronger.
 */
export function axisGridlines(t0: number, t1: number): { ts: number; isDay: boolean }[] {
  const span = t1 - t0;
  if (!(span > 0)) return [];
  return roundTimes(t0, t1, pickStep(span, MAX_TICK_INTERVALS * 2)).map((ts) => ({
    ts,
    isDay: ts === localDayStart(ts),
  }));
}

/**
 * Per-PC operational swimlane. Folds a window of raw operational events
 * (power / session / sleep kinds) into lane spans the operator can read at a
 * glance: when the host was up, who was signed in, when it slept. Shared by
 * the Analytics `op_timeline` widget (which receives a server-computed
 * window) and the Events page strip (which passes the selected period).
 * The active/idle lane is fed by the agent-native idle
 * sampler (#841); a filled span = active, a gap = idle. The active /
 * session / sleep lanes are clipped to the power lane's ON spans so a state
 * left open across a power cycle can't paint over a powered-off gap. When a
 * PC has no winlog power events at all, the power and session lanes are
 * instead backfilled from the active/idle presence envelope — the sampler
 * running (active OR idle) proves the host was up and signed in, so those
 * lanes stay filled across idle stretches (#970).
 *
 * `from` / `to` bound the window; when omitted they fall back to the
 * earliest / latest event so the Events page can use it without a window.
 */
/**
 * `ranges` minus every interval in `cut`, keeping the leftovers in order.
 *
 * The no-evidence band is per lane, not per strip: drawing one band across
 * all four and letting spans paint over it looked right in principle and
 * wrong on screen. The unconfirmed hatch is a *gradient with transparent
 * gaps*, so a band underneath shows through them — the two patterns
 * interleave into a plaid, and a stretch where the lane does have evidence
 * ends up asserting "believed on" and "no evidence" in the same pixels.
 * Subtracting first means every pixel carries exactly one statement.
 */
export function subtractRanges<T extends { from: number; to: number }>(
  ranges: T[],
  cut: { from: number; to: number }[],
): T[] {
  let out = ranges.filter((r) => r.to > r.from);
  for (const c of cut) {
    if (c.to <= c.from) continue;
    const next: T[] = [];
    for (const r of out) {
      if (c.to <= r.from || c.from >= r.to) {
        next.push(r); // disjoint
        continue;
      }
      // Splitting keeps the source range's other fields (its `reason`), so a
      // leftover sliver still explains itself in the tooltip.
      if (c.from > r.from) next.push({ ...r, to: c.from });
      if (c.to < r.to) next.push({ ...r, from: c.to });
    }
    out = next;
  }
  return out;
}

/** Why a stretch carries no evidence. */
export type NoEvidenceReason = 'truncated' | 'offline';

/**
 * The stretches the strip must not make any claim about. Two causes, one
 * meaning — "no evidence either way":
 *
 *   [t0, coverEdge)          the fetch was truncated before reaching here
 *   [certainEdge, liveEdge]  the agent stopped reporting
 *
 * Returned non-overlapping and in order. Overlap is not hypothetical: the
 * coverage floor is global across PCs, so a host that went quiet days ago
 * sits behind a floor set by a busier host's recent events and its
 * `certainEdge` lands before `coverEdge`; a never-reported agent with no
 * events puts `certainEdge` at 0 and overlaps outright. Two entries over the
 * same pixels would stack two elements and leave which `reason` the tooltip
 * shows to paint order.
 */
export function noEvidenceRanges(
  t0: number,
  t1: number,
  now: number,
  coverEdge: number,
  lastHeartbeat?: string | null,
  lastEventTs?: number,
): { from: number; to: number; reason: NoEvidenceReason }[] {
  const { certainEdge, liveEdge } = evidenceEdges(t1, now, lastHeartbeat, lastEventTs);
  const out: { from: number; to: number; reason: NoEvidenceReason }[] = [];
  if (coverEdge > t0) out.push({ from: t0, to: coverEdge, reason: 'truncated' });
  // Clamped past the truncated stretch, not merely past the window start.
  // `coverEdge >= t0` always, so this subsumes the window-start clamp.
  const from = Math.max(certainEdge, coverEdge);
  if (liveEdge > from) out.push({ from, to: liveEdge, reason: 'offline' });
  return out;
}

/**
 * Newest parseable event instant, or `undefined` for an empty set. `reduce`
 * rather than `Math.max(...)`: the spread would blow the call stack on the
 * unbounded event sets the Analytics query can return.
 */
export function newestEventTs(events: OpEvent[]): number | undefined {
  let hi = -Infinity;
  for (const e of events) {
    const ts = Date.parse(e.at);
    if (!Number.isNaN(ts) && ts > hi) hi = ts;
  }
  return Number.isFinite(hi) ? hi : undefined;
}

/**
 * The two edges that bound what the strip may assert.
 *
 * `liveEdge` is the newest instant that has happened — an open span clamps
 * there rather than running to a window end in the future. `certainEdge` is
 * the newest instant the agent has vouched for: past `lastHeartbeat + grace`
 * we have no evidence of anything, because the agent stopped reporting.
 *
 * `grace` is the fleet-wide online/offline threshold, so one missed beat on a
 * healthy agent doesn't fray the strip's tail. Without a heartbeat the two
 * edges coincide and nothing is gated.
 *
 * Exported because the renderer needs the same edges the lane builder used:
 * `gateToHeartbeat` can only hatch spans that exist, and the whole point of
 * the no-evidence band is to cover the lanes that have *no* span there.
 */
export function evidenceEdges(
  t1: number,
  now: number,
  lastHeartbeat?: string | null,
  // Newest event actually received for this PC. Only consulted when the
  // server has told us the agent has *never* reported a heartbeat (see
  // below), where it is the only thing that bounds what we can vouch for.
  lastEventTs?: number,
): { liveEdge: number; certainEdge: number } {
  const liveEdge = Math.min(t1, now);

  // `undefined` and `null` are NOT the same thing here, and collapsing them
  // was a real bug: `undefined` means we haven't been told yet (the
  // heartbeat query is in flight, or this PC has no agent row) and gating on
  // that would hatch every healthy strip on each page load; `null` means the
  // server told us this agent registered and has never once reported.
  // `agents.last_heartbeat` is genuinely nullable — `cleanup.rs` exempts
  // exactly those rows from pruning — so this is a reachable state, and it
  // is precisely the case that most needs the honesty this component is
  // supposed to provide.
  if (lastHeartbeat === undefined) return { liveEdge, certainEdge: liveEdge };

  if (lastHeartbeat === null) {
    // Never reported. Events still prove the host was alive when it sent
    // them — they arrive over durable JetStream, unlike the lossy heartbeat
    // — so certainty runs to the newest event we hold and no further.
    // Extrapolating an open span past that would assert liveness nothing has
    // ever evidenced. With no events either, the whole window is unknown.
    return {
      liveEdge,
      certainEdge: lastEventTs === undefined ? 0 : Math.min(liveEdge, lastEventTs),
    };
  }

  const hbMs = Date.parse(lastHeartbeat);
  return {
    liveEdge,
    certainEdge: Number.isNaN(hbMs)
      ? liveEdge
      : Math.min(liveEdge, hbMs + AGENT_ACTIVE_THRESHOLD_MS),
  };
}

/**
 * Derive the four lanes from a window of raw operational events.
 *
 * Pure and exported so the lane dependency rules can be tested directly:
 * the interactions between power, session, sleep and active are subtle
 * enough that they have been fixed four times (#841, #972, #981, #983, and
 * the session backfill gate below), each time from a screenshot rather than
 * a failing test. `now` is a parameter for the same reason.
 */
export function buildLanes(
  events: OpEvent[],
  windowFrom: number,
  t1: number,
  now: number,
  lastHeartbeat?: string | null,
  // Oldest instant the event set actually covers. Set when the fetch was
  // truncated by `limit` (which drops the OLD end of the window — the backend
  // orders `at DESC`), so the window extends back further than the data does.
  //
  // Everything is then reconstructed from this floor rather than from `t0`,
  // because `buildSpans`' carry-in would otherwise fabricate the uncovered
  // region wholesale: the oldest surviving event is typically an end with no
  // matching start, which reads as "this state was already open at t0" and
  // paints the entire gap. On a 2-day window truncated to the last few hours
  // that renders as two solid days of "the user was at this machine" — a
  // stronger and more wrong claim than anything this component is trying to
  // fix. The caller paints [t0, coverageFrom) as no-data instead.
  coverageFrom?: number,
): { key: LaneKey; color: string; spans: Span[]; markers: { ts: number; kind: string }[] }[] {
  if (t1 <= windowFrom) return [];
  // Floor for span reconstruction. The axis still spans [windowFrom, t1];
  // only the spans are held back to what the data can support.
  const t0 =
    coverageFrom === undefined ? windowFrom : Math.min(Math.max(coverageFrom, windowFrom), t1);
  if (t1 <= t0) return [];
  const { liveEdge, certainEdge } = evidenceEdges(t1, now, lastHeartbeat, newestEventTs(events));
  // Reconstruct the power lane first: it's the ground truth for "the host
  // was up", and the subordinate lanes get clipped to its ON spans below.
  const power = OP_LANES.find((l) => l.key === 'power')!;
  const powerSpans = buildSpans(events, power.starts, power.ends, t0, t1, now);
  const onIntervals = powerSpans.map((s) => ({ from: s.from, to: s.to }));
  // Only clip when we actually have power events — a PC that reports just
  // active/idle (no winlog power lane) has no ON spans, and clipping to an
  // empty set would erase its only signal.
  const powerKinds = new Set<string>([...power.starts, ...power.ends]);
  // Note this is kind-membership over whatever `events` holds, which a
  // truncated fetch can change. If truncation drops every power event for a
  // host while keeping its recent session/active ones — plausible, since the
  // last reboot may be days old while sign-in and sampler traffic is
  // constant — `hasPower` flips false and the covered region silently
  // switches from winlog reconstruction to the presence-envelope backfill.
  // That isn't fabrication (the envelope is sampler-evidenced, the same
  // inference #970 already rests on), but it is a different *method* inside
  // the region the strip presents as covered, and nothing distinguishes it
  // from a genuinely winlog-less host. Detecting the difference would need a
  // second query ("does this PC have power events older than the floor?"),
  // so it is documented rather than handled.
  const hasPower = events.some((e) => powerKinds.has(e.kind));
  const session = OP_LANES.find((l) => l.key === 'session')!;
  // The active lane (agent idle sampler): a filled span = active, a gap =
  // idle. Built once and shown as-is on its own lane.
  const active = OP_LANES.find((l) => l.key === 'active')!;
  const activeSpans = buildSpans(events, active.starts, active.ends, t0, t1, now);
  // #970 backfill: when the PC reports NO winlog power events at all (the
  // winlog collector isn't running on that host), the idle sampler is the
  // only signal that the host was up. Paint the power and session lanes from
  // it so those lanes aren't blank while active fills, keeping the
  // "active ⇒ power & session" invariant.
  //
  // Backfill from the *presence envelope*, NOT the active spans: an `idle`
  // event still means the agent was sampling, i.e. the host was powered on
  // and the user signed in — just not typing. Painting power/session from the
  // active spans directly made them drop out during every idle stretch, as if
  // the box powered off or logged out the moment the user paused (the minipc
  // report that surfaced this). Union the active spans (active→idle) with the
  // idle spans (idle→active) and coalesce: that collapses the sampler's whole
  // run into one continuous band across the idle gaps while keeping
  // buildSpans' carry-in at both window edges, so the envelope reaches the
  // edge instead of starting at the first raw event and leaving a sliver where
  // the active lane's own carry-in already paints (power/session ⊇ active,
  // exactly). The band runs to the live edge and is trimmed / hatched there by
  // the heartbeat gate below when the agent is offline. The active lane itself
  // still shows the idle gaps.
  const idleSpans = buildSpans(events, active.ends, active.starts, t0, t1, now);
  const presenceSpans = mergeSpans([...activeSpans, ...idleSpans]);
  // Scoped to the absent-winlog case on purpose: once winlog exists it stays
  // authoritative — including its OFF gaps — so a stale open `active` carried
  // across a power cycle can't union its way over a powered-off span (#841).
  // The clip-to-power path is unchanged there.
  //
  // The session backfill unions the genuine spans with the envelope instead
  // of choosing one or the other. It used to be gated on
  // `!hasPower && !hasSession`, which was all-or-nothing on a lane where
  // partial data is the norm: a single logon anywhere in the window flipped
  // `hasSession` and disabled the backfill for the *whole* window, leaving
  // one short span on an otherwise blank lane while power and active were
  // filled end to end — the "active but no session" report, and a visible
  // break of the invariant #970 set out to hold. Unioning keeps what that
  // gate was protecting (genuine spans are never overwritten, and they still
  // back the lane's logon/logoff markers) while the envelope covers the
  // stretches winlog says nothing about. Note the envelope subsumes a
  // genuine logoff gap when the sampler kept running across it — the sampler
  // runs as the signed-in user, so its output is itself evidence of a
  // session; the logoff marker stays on the lane either way.
  const sessionSpans = buildSpans(events, session.starts, session.ends, t0, t1, now);
  return OP_LANES.map((lane) => {
    let spans: Span[];
    if (lane.key === 'power') {
      spans = hasPower ? powerSpans : presenceSpans;
    } else if (lane.key === 'session') {
      // Both session paths in one branch so the generic branch below doesn't
      // rebuild the spans `sessionSpans` already holds.
      spans = hasPower
        ? clipToOn(sessionSpans, onIntervals)
        : mergeSpans([...sessionSpans, ...presenceSpans]);
    } else if (lane.key === 'active') {
      // Reuse the pre-built active spans; still clip to power ON when winlog
      // exists so a stale open span can't paint across a powered-off gap.
      spans = hasPower ? clipToOn(activeSpans, onIntervals) : activeSpans;
    } else {
      spans = buildSpans(events, lane.starts, lane.ends, t0, t1, now);
      if (hasPower) spans = clipToOn(spans, onIntervals);
    }
    // Hatch any ongoing state past the agent's last heartbeat as unconfirmed.
    spans = gateToHeartbeat(spans, certainEdge, liveEdge);
    return {
      key: lane.key as LaneKey,
      color: lane.color,
      spans,
      markers: laneMarkers(events, lane.starts, lane.ends, t0, t1),
    };
  });
}

export function OperationalTimeline({
  events,
  from,
  to,
  lastHeartbeat,
  coverageFrom,
}: {
  events: OpEvent[];
  from?: string;
  to?: string;
  // Oldest instant the event set actually covers (ISO), when the fetch was
  // truncated by `limit`. The window before it renders as "no data" instead
  // of being extrapolated from the oldest surviving event.
  coverageFrom?: string;
  // The agent's last heartbeat (ISO). When present, the strip stops painting a
  // lane's ongoing state past `lastHeartbeat + grace` and hatches that tail as
  // unconfirmed — the agent is offline, so we can't vouch for the current
  // state. Omitted (Events page) → no gating, solid strips as before.
  lastHeartbeat?: string | null;
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

  // Where the data actually starts, clamped into the window. Anything left of
  // it is uncovered: the fetch was truncated before reaching back this far.
  const coverEdge = useMemo(() => {
    if (!coverageFrom) return t0;
    const ms = Date.parse(coverageFrom);
    return Number.isNaN(ms) ? t0 : Math.min(Math.max(ms, t0), t1);
  }, [coverageFrom, t0, t1]);

  // Snapshot "now" once per render pass so open intervals don't paint past
  // the present when the window runs into the future (today before midnight).
  // Held in a memo on the same deps as the lanes so the band below is drawn
  // against the identical instant the spans were built from — recomputing
  // `Date.now()` separately would let the two drift by a render.
  // biome-ignore lint/correctness/useExhaustiveDependencies: the deps are the
  // inputs whose change should re-anchor "now"; Date.now() has none of its own.
  const now = useMemo(() => Date.now(), [events, t0, t1, lastHeartbeat, coverEdge]);

  const lanes = useMemo(
    () => buildLanes(events, t0, t1, now, lastHeartbeat, coverEdge),
    [events, t0, t1, now, lastHeartbeat, coverEdge],
  );

  // Stretches the strip must not make any claim about. Two causes, one
  // meaning — "no evidence either way":
  //   [t0, coverEdge)          the fetch was truncated before reaching here
  //   [certainEdge, liveEdge]  the agent stopped reporting
  // Painted as a neutral band so an *empty* lane in these stretches reads as
  // unknown rather than as a confident "this state was off". Without it the
  // absence of a span is indistinguishable from a measured absence — the
  // asymmetry that made an offline agent's idle lane assert "nobody was
  // here" for the entire outage (#1086).
  const noEvidence = useMemo(
    () => noEvidenceRanges(t0, t1, now, coverEdge, lastHeartbeat, newestEventTs(events)),
    [events, t0, t1, now, lastHeartbeat, coverEdge],
  );


  const ticks = useMemo(() => axisTicks(t0, t1), [t0, t1]);
  const gridlines = useMemo(() => axisGridlines(t0, t1), [t0, t1]);

  // Crosshair: the instant under the pointer, or null when it's away. Reading
  // four lanes at one moment otherwise means eyeballing a vertical line across
  // four separate tracks and hoping you stayed on the same x.
  const [hoverTs, setHoverTs] = useState<number | null>(null);
  // Measures the track column (everything right of the lane labels). The lanes
  // and the axis share its geometry, so one rect converts pointer x to a time
  // for all of them.
  const trackRef = useRef<HTMLDivElement>(null);

  const onMove = (e: ReactMouseEvent<HTMLDivElement>) => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect || rect.width <= 0) return;
    const frac = (e.clientX - rect.left) / rect.width;
    // Off to the side of the tracks (over the labels, or past the right edge):
    // no meaningful instant, so drop the crosshair rather than clamp it to an
    // edge time the pointer isn't actually on.
    setHoverTs(frac < 0 || frac > 1 ? null : t0 + frac * (t1 - t0));
  };

  if (t1 <= t0) {
    return <div className="text-muted text-sm">{t('opTimeline.empty')}</div>;
  }

  const span = t1 - t0;
  const pct = (ts: number) => ((ts - t0) / span) * 100;

  // What each lane asserts at the crosshair. `uncertain` propagates so the
  // readout can't state a hatched (unconfirmed) span as fact.
  const at =
    hoverTs === null
      ? null
      : lanes.map((lane) => {
          const s = lane.spans.find((x) => x.from <= hoverTs && hoverTs < x.to);
          // No span AND inside a no-evidence stretch → unknown, not "off".
          // Reporting "off" here is the exact failure this change exists to
          // remove: an absent span is only meaningful where we were listening.
          const blind =
            !s && noEvidence.some((n) => n.from <= hoverTs && hoverTs < n.to);
          return {
            key: lane.key,
            color: lane.color,
            on: !!s,
            uncertain: !!s?.uncertain,
            unknown: blind,
          };
        });

  return (
    <div
      className="relative space-y-1.5"
      onMouseMove={onMove}
      onMouseLeave={() => setHoverTs(null)}
    >
      {lanes.map((lane) => (
        <div key={lane.key} className="flex items-center" style={{ gap: LANE_LABEL_GAP_PX }}>
          <div
            className="flex shrink-0 items-center gap-1.5 text-xs text-muted"
            style={{ width: LANE_LABEL_WIDTH_PX }}
          >
            <span
              className="inline-block size-2 shrink-0 rounded-sm"
              style={{ backgroundColor: lane.color }}
            />
            <span className="truncate">{t(`opTimeline.lanes.${lane.key}`)}</span>
          </div>
          <div className="relative h-6 flex-1 overflow-hidden rounded-sm bg-muted/10">
            {/* No-evidence band, drawn FIRST so spans paint over it: where a
                lane does have a span here it keeps its own rendering (a
                lane-coloured hatch, for a state believed but unconfirmed),
                and only the genuinely blank parts show as unknown.

                Distinguished from the unconfirmed hatch by GEOMETRY, not just
                colour: opposite angle (135° vs 45°) and a wider period (8px
                vs 6px). Relying on hue alone would have asked the viewer to
                tell neutral grey at 28% from sky-blue at 35% — a distinction
                I could not verify and would not bet a semantic on. The
                opposing angle also stops the two patterns beating into a
                moiré where an uncertain span's left edge doesn't align with
                the band's, which same-angle stripes at different phases do. */}
            {subtractRanges(noEvidence, lane.spans).map((n, i) => (
              <div
                key={`n-${i}`}
                // No `pointer-events-none`, unlike the gridlines and the
                // crosshair line: this element carries a `title` explaining
                // why the stretch is unknown, and a native tooltip needs the
                // element to actually receive the hover. With pointer events
                // off the message could never appear — the same reason the
                // spans and markers below don't disable them either. The
                // crosshair still works: `onMouseMove` is on the outer
                // container and mouse events bubble.
                className="absolute top-0 h-full"
                style={{
                  left: `${pct(n.from)}%`,
                  width: `${Math.max(pct(n.to) - pct(n.from), 0)}%`,
                  backgroundImage:
                    'repeating-linear-gradient(135deg, rgb(127 127 127 / 0.3) 0, rgb(127 127 127 / 0.3) 3px, transparent 3px, transparent 8px)',
                }}
                title={t(`opTimeline.noEvidence.${n.reason}`, {
                  from: fmtIsoLocal(new Date(n.from).toISOString()),
                  to: fmtIsoLocal(new Date(n.to).toISOString()),
                })}
              />
            ))}
            {lane.spans.map((s, i) => {
              const laneName = t(`opTimeline.lanes.${lane.key}`);
              const from = fmtIsoLocal(new Date(s.from).toISOString());
              const to = fmtIsoLocal(new Date(s.to).toISOString());
              // Unconfirmed tail (agent offline): hatch it instead of a solid
              // fill so it reads as "believed, not verified", never a claim
              // the host was definitely up.
              if (s.uncertain) {
                return (
                  <div
                    key={i}
                    className="absolute top-0 h-full"
                    style={{
                      left: `${pct(s.from)}%`,
                      width: `${Math.max(pct(s.to) - pct(s.from), 0.4)}%`,
                      backgroundImage: `repeating-linear-gradient(45deg, ${lane.color}59 0, ${lane.color}59 3px, transparent 3px, transparent 6px)`,
                      borderTopLeftRadius: 0,
                      borderBottomLeftRadius: 0,
                      borderTopRightRadius: s.openEnd ? 0 : 2,
                      borderBottomRightRadius: s.openEnd ? 0 : 2,
                    }}
                    title={t('opTimeline.uncertainTooltip', { lane: laneName, from, to })}
                  />
                );
              }
              const note = [
                s.openStart ? t('opTimeline.openStart') : '',
                s.openEnd ? t('opTimeline.openEnd') : '',
              ]
                .filter(Boolean)
                .join(' · ');
              const title =
                t('opTimeline.spanTooltip', { lane: laneName, from, to }) +
                (note ? ` (${note})` : '');
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
                    // open span reads as "ongoing", not a hard boundary. A
                    // heartbeat-split head keeps a square right edge so it
                    // abuts its hatched tail seamlessly.
                    borderTopLeftRadius: s.openStart ? 0 : 2,
                    borderBottomLeftRadius: s.openStart ? 0 : 2,
                    borderTopRightRadius: s.openEnd || s.cutByHeartbeat ? 0 : 2,
                    borderBottomRightRadius: s.openEnd || s.cutByHeartbeat ? 0 : 2,
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
            {/* Gridlines on every axis tick, drawn last so they stay visible
                over a filled span — a lane painted end to end would otherwise
                swallow them and there'd be nothing to read a position
                against. Day boundaries are stronger than the hour ticks, so
                the date structure still reads at a glance without the
                intermediate lines competing with the spans. */}
            {gridlines.map((g, i) => (
              <div
                key={`g-${i}`}
                className={`pointer-events-none absolute top-0 h-full w-px ${
                  g.isDay ? 'bg-fg/40' : 'bg-fg/15'
                }`}
                style={{ left: `${pct(g.ts)}%` }}
              />
            ))}
            {hoverTs !== null && (
              <div
                className="pointer-events-none absolute top-0 h-full w-px bg-fg/80"
                style={{ left: `${pct(hoverTs)}%` }}
              />
            )}
          </div>
        </div>
      ))}

      {/* Time axis. Also the measuring stick for the crosshair: it shares the
          lanes' left offset and width, so its rect converts pointer x to a
          time without threading a ref through every lane track. */}
      <div
        ref={trackRef}
        className="relative h-4 text-[9px] text-muted"
        style={{ marginLeft: TRACK_OFFSET_PX }}
      >
        {ticks.map((tick, i) => {
          const p = pct(tick.ts);
          return (
            <span
              key={i}
              className={`absolute whitespace-nowrap ${tick.isDay ? 'font-medium text-fg/80' : ''}`}
              style={{
                left: `${p}%`,
                // Centre the label on its tick, except near the edges where a
                // centred label would spill outside the strip.
                transform:
                  p < 4 ? 'translateX(0)' : p > 96 ? 'translateX(-100%)' : 'translateX(-50%)',
              }}
            >
              {tick.label}
            </span>
          );
        })}
      </div>

      {/* Crosshair readout: every lane's state at one instant, so the four
          tracks can be read as a single moment instead of by eye across four
          rows. Sits *below* the axis rather than next to the pointer — the
          lanes are only 24px tall, so a floating box overlaps the very spans
          the crosshair is asking about. */}
      {hoverTs !== null && at && (
        <div
          className="pointer-events-none absolute top-full z-10 mt-1 rounded-sm border border-border bg-card px-2 py-1.5 text-[10px] shadow-lg"
          style={{
            // Positioned against the whole strip, but the tracks start
            // `TRACK_OFFSET_PX` in, so the fraction applies to the track width
            // rather than to `100%`. (The crosshair's own `left` is a plain
            // percentage because it lives *inside* a track, a different
            // coordinate space that needs no offset.)
            left: `calc(${TRACK_OFFSET_PX}px + (100% - ${TRACK_OFFSET_PX}px) * ${pct(hoverTs) / 100})`,
            // Centred on the crosshair, pinned back at the edges so it can't
            // spill out of the card.
            transform:
              pct(hoverTs) < 15
                ? 'translateX(0)'
                : pct(hoverTs) > 85
                  ? 'translateX(-100%)'
                  : 'translateX(-50%)',
          }}
        >
          <div className="mb-1 font-medium text-fg whitespace-nowrap">
            {fmtIsoLocal(new Date(hoverTs).toISOString())}
          </div>
          {at.map((l) => (
            <div key={l.key} className="flex items-center gap-1.5 whitespace-nowrap">
              <span
                className="inline-block size-2 shrink-0 rounded-sm"
                style={{
                  backgroundColor: l.color,
                  // Unknown gets its own weight, between on and off: the dot
                  // must not read as a confident "off".
                  opacity: l.on ? 1 : l.unknown ? 0.5 : 0.25,
                }}
              />
              <span className="text-muted">{t(`opTimeline.lanes.${l.key}`)}</span>
              <span className={l.on || l.unknown ? 'text-fg' : 'text-muted'}>
                {l.unknown
                  ? t('opTimeline.unknown')
                  : t(`opTimeline.state.${l.key}.${l.on ? 'on' : 'off'}`)}
                {l.uncertain && ` (${t('opTimeline.unconfirmed')})`}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
