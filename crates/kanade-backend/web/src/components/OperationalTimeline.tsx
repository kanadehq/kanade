/**
 * The operational swimlane, and the evidence rules it draws.
 *
 * # What a timestamp means
 *
 * Three producers write the events this file reads, and an `at` means
 * something different in each. Nearly every bug this component has had came
 * from treating one as another.
 *
 * | `source`     | who wrote it        | `at` dates      | proves at `at`        |
 * |--------------|---------------------|-----------------|-----------------------|
 * | `winlog:*`   | Windows, at the time| the MACHINE     | the host was running  |
 * | `agent:*`    | the agent, running  | the AGENT       | the agent was running |
 * | `backend:*`  | the heartbeat watchdog | the CONNECTION | what the backend heard |
 *
 * Delivery is not dating. Winlog records are read out of the Event Log after
 * the fact — up to 24 h of backfill — so their arrival says nothing about
 * when the host was alive, only their `at` does (#1245). Conversely an
 * `agent:*` record proves the agent was up at its own `at` however late it
 * lands, which is what lets it close an unknown stretch.
 *
 * One kind breaks the first row and has to be special-cased: see `marks` on
 * the power lane.
 *
 * # What a pixel may say
 *
 * | rendering            | claim                          | source            |
 * |----------------------|--------------------------------|-------------------|
 * | solid lane colour    | this state, measured           | a span            |
 * | lane-coloured hatch  | this state, believed unconfirmed | `Span.uncertain`|
 * | blank                | NOT this state, measured       | absence of a span |
 * | grey hatch (135°)    | no evidence either way         | `noEvidenceRanges`|
 *
 * Blank is a claim. That is the asymmetry every fix here turns on (#1086): an
 * empty lane is only meaningful where we were listening, so anywhere we were
 * not has to be hatched instead of left to read as a confident "off".
 *
 * # When a lane may claim an interval
 *
 * | the pair                    | claims its interval because | may bridge silence |
 * |-----------------------------|-----------------------------|--------------------|
 * | `boot`→`shutdown`           | the OS logged both ends     | yes                |
 * | `sleep`→`resume`            | the OS logged both ends     | yes                |
 * | `logon`→`logoff`            | the OS logged both ends     | yes                |
 * | `active`→`idle`             | no transition was sampled   | NO — a dead agent  |
 * |                             |                             | samples nothing    |
 * | anything with no closing record | the renderer ran it to the edge | NO          |
 *
 * The last row is a property of the SPAN, not of the lane: an `openEnd` power
 * span is a reconstruction whatever produced its start, and an unclean
 * power-off is exactly the case that leaves one (`buildLanes`' outage cut).
 *
 * # When an unknown stretch ends
 *
 * At the first proof the agent was reporting again: the watchdog's own
 * `agent_online`, or any `agent:*` record. NOT a `winlog:*` record. The
 * watchdog's close is the one that can also be missing — `open` outages live
 * in backend process memory, so a restart mid-outage never writes it — which
 * is why the agent's own records have to count (`agentDownRanges`).
 *
 * # Where each rule lives
 *
 *   buildSpans                pair start/end kinds into intervals
 *   unrecordedShutdownRanges  a `boot` on an open span ⇒ an undated power cycle
 *   clipToOn                  subordinate lanes ⊆ power ON
 *   cutAtContradiction        a host record inside a `sleep` disproves it
 *   gateToHeartbeat           ongoing state past the last heartbeat is hatched
 *   agentDownRanges           recorded outages, and what closes them
 *   noEvidenceRanges          every stretch nobody can account for
 *   noEvidenceByLane          …narrowed to what each lane actually doesn't know
 *   recordedIntervals         off/asleep intervals the OS bounded at BOTH ends
 *
 * The matrix these implement is exercised end to end in
 * `OperationalTimeline.test.ts`; add the row there before changing a rule
 * here.
 */
import { type MouseEvent as ReactMouseEvent, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { AGENT_ACTIVE_THRESHOLD_MS, fmtIsoLocal } from '@/lib/utils';

// One raw operational event. Mirrors the backend `OpEvent`
// (api/analytics.rs) and the per-PC rows the Events page already holds.
export type OpEvent = {
  at: string;
  kind: string;
  /** `<scheme>:<detail>` of the collector that produced it, e.g.
   *  `winlog:System` or `agent:internal`. Optional so a caller that only has
   *  `{at, kind}` still type-checks; `hasWinlog` falls back to a kind test
   *  when it is absent. */
  source?: string;
  /** OS boot time (epoch **seconds**) — set only on an `agent:startup`
   *  event. What lets an outage be attributed rather than merely marked
   *  (#1316); absent on every other kind and on anything an agent older
   *  than that wrote. */
  bootTime?: number | null;
};

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
//
// `marks` is a third category: records that belong on the lane and must be
// shown, but that assert no transition AT THEIR OWN TIMESTAMP. Every kind in
// `starts`/`ends` is stamped when the thing it describes happened; a `marks`
// kind is not, so pairing it would date a transition from the wrong instant.
const OP_LANES = [
  {
    key: 'power',
    starts: ['boot', 'log_service_started'],
    ends: ['shutdown', 'log_service_stopped'],
    // `unexpected_shutdown` is Kernel-Power 41, and Windows writes it during
    // the kernel phase of the NEXT boot — the flag saying "the last session
    // did not end cleanly" is only readable once the machine is back. The
    // collector takes `at` straight from the record's `TimeCreated`
    // (winlog.rs), so its timestamp is when the host CAME BACK, never when it
    // went away.
    //
    // Pairing it as a power END therefore closed the previous session at the
    // following morning's boot and painted the entire dark night solid ON —
    // as a closed start/end pair, so nothing downstream questioned it: not
    // the heartbeat gate (it only hatches open spans), not the outage cut (it
    // only cuts spans that rest on the sampler's silence). That is the
    // reported "電源を落としているのに夜間ずっと塗られている".
    //
    // It stays on the lane as a marker, because "the last power-off was
    // unclean" is exactly what an operator wants to see at that boot. The off
    // interval it implies is reconstructed from the unpaired `boot` instead,
    // which is the one record that does carry a trustworthy instant —
    // see `unrecordedShutdownRanges`.
    marks: ['unexpected_shutdown'],
    color: '#10b981', // emerald-500
  },
  {
    key: 'session',
    starts: ['logon'],
    ends: ['logoff'],
    marks: [],
    color: '#8b5cf6', // violet-500
  },
  {
    key: 'sleep',
    starts: ['sleep'],
    ends: ['resume'],
    marks: [],
    color: '#f59e0b', // amber-500
  },
  {
    key: 'active',
    starts: ['active'],
    ends: ['idle'],
    marks: [],
    color: '#0ea5e9', // sky-500
  },
] as const;

type LaneKey = (typeof OP_LANES)[number]['key'];

// Kinds that describe whether the fleet was being *observed*, as opposed to
// what a host was doing. They drive no lane — there is no "the agent was
// running" row — but they bound what every other lane may claim, so the strip
// needs them alongside the lane kinds.
//
// `agent_offline` is written by the backend when a host's heartbeats stop
// (backdated to the first beat that failed to arrive). Unlike the live-edge
// gate, which can only ever describe *now*, these are durable events with a
// real `at`, so a stretch that has since become history still reads as
// unknown instead of reverting to a confident-looking gap.
// `agent_online` carries no range of its own — it exists to close the one
// `agent_offline` opened. Without it the stretch would run to the next event
// of any kind, and obs_events are transition-driven, so a recovered host that
// simply stays idle produces none for a long while and the outage would read
// as far longer than it was.
const OP_OBSERVATION_KINDS = ['agent_offline', 'agent_online'] as const;
const OP_OBSERVATION_KIND_SET: ReadonlySet<string> = new Set(OP_OBSERVATION_KINDS);

// Every kind the strip reads — the Events page filters its rows down to these
// before handing them over, and it matches the backend `op_timeline` query's
// IN-list.
export const OP_TIMELINE_KINDS: readonly string[] = [
  ...OP_LANES.flatMap((l) => [...l.starts, ...l.ends, ...l.marks]),
  ...OP_OBSERVATION_KINDS,
];

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
// `kinds` is the lane's starts, ends AND marks: a `marks` kind drives no
// transition, so the marker is the only way it reaches the strip at all.
function laneMarkers(
  events: OpEvent[],
  kinds: readonly string[],
  t0: number,
  t1: number,
): { ts: number; kind: string }[] {
  const keep = new Set(kinds);
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

/**
 * Extent of a set of events, padded 2% (at least a minute) so end markers
 * aren't flush against the strip edge. `undefined` when there is nothing to
 * derive a window from.
 */
function paddedExtent(
  events: readonly { at: string }[],
): [string, string] | undefined {
  let lo = Infinity;
  let hi = -Infinity;
  for (const e of events) {
    const ts = Date.parse(e.at);
    if (Number.isNaN(ts)) continue;
    if (ts < lo) lo = ts;
    if (ts > hi) hi = ts;
  }
  if (!Number.isFinite(lo) || !Number.isFinite(hi) || hi <= lo) return undefined;
  const pad = Math.max(60_000, (hi - lo) * 0.02);
  return [new Date(lo - pad).toISOString(), new Date(hi + pad).toISOString()];
}

/**
 * The window every strip shares, so their axes line up.
 *
 * The selected period wins outright when it has a lower bound: deriving the
 * window from the data instead made the axis lie about the filter (an axis
 * reading 7/18 23:24 for a period beginning 7/19 00:00), and worse under
 * `limit` truncation, where the lanes silently showed a few hours of a
 * multi-day selection.
 *
 * Without one (the `all` preset) the extent has to come from the data, and
 * the fallback order matters. Preferring the kept PCs' operational events is
 * right when they exist. When they don't — a pinned host that has none — that
 * path yields no bounds, and the strip then has no axis, trips its own
 * `t1 <= t0` guard and renders the plain "no events" note instead of the
 * unknown hatching. That is the pin achieving nothing on precisely the preset
 * an operator picks to ask "did this host EVER report". So it falls back to
 * the whole response's extent: other kinds fetched for the same host still
 * say when it was around, which is a truthful axis to hatch against. Bounds
 * are never invented — a host with no events of any kind yields none, and the
 * note is then the correct answer rather than a fallback.
 */
export function swimlaneWindow(
  windowFrom: string | undefined,
  windowTo: string | undefined,
  pcs: readonly string[],
  opEvents: readonly { at: string; pc_id: string }[],
  allEvents: readonly { at: string }[],
): [string | undefined, string | undefined] {
  if (windowFrom) return [windowFrom, windowTo];
  const kept = new Set(pcs);
  return (
    paddedExtent(opEvents.filter((e) => kept.has(e.pc_id))) ??
    paddedExtent(allEvents) ?? [undefined, undefined]
  );
}

/** Why a stretch carries no evidence. */
export type NoEvidenceReason =
  | 'truncated'
  | 'offline'
  | 'noEvents'
  /** Heard nothing, and cannot say why — no `agent:startup` carrying a boot
   *  time closed this stretch. What every outage read as before #1316, and
   *  still the answer for agents that predate it. */
  | 'agentDown'
  /** The machine itself was away: the agent came back reporting a boot
   *  LATER than the last heartbeat, so the host rebooted during the gap. */
  | 'agentDownHostOff'
  /** Only the agent was away: it came back reporting a boot EARLIER than the
   *  last heartbeat, so the machine never rebooted and was up throughout. */
  | 'agentDownAgentStopped'
  /** Only the connection was away: the outage closed with no agent restart
   *  at all, so the agent was running and observing the whole time — its own
   *  records for the stretch arrive later through the outbox. */
  | 'agentDownLinkOnly'
  /** The host came back up and nothing recorded it going down. A `boot`
   *  arrived while the power lane already believed the machine was ON, which
   *  proves a power cycle happened and that no `shutdown` record dates it —
   *  see `unrecordedShutdownRanges`. */
  | 'unrecordedShutdown';

/**
 * Unknown stretches recorded by `agent_offline` events.
 *
 * These answer the question the live-edge gate structurally cannot: it can
 * only say "I do not know about *now*", because a heartbeat is a single
 * overwritten column with no history. An `agent_offline` event is durable and
 * carries a real `at`, so an outage that has since become history still reads
 * as unknown rather than reverting to a gap indistinguishable from measured
 * idle — the #1089 asymmetry.
 *
 * Each event opens a stretch that ends at the first instant something proves
 * the AGENT was running again — see the two kinds of proof at the scan below.
 * The `agent_online` the watchdog writes is one of them but not the only one,
 * which matters: it is written from in-memory state that a backend restart
 * drops, so an outage that spanned a deploy never receives one at all.
 *
 * `events` must be the host's events, and is not assumed sorted.
 */
export function agentDownRanges(
  events: OpEvent[],
  t0: number,
  t1: number,
  liveEdge: number,
): { from: number; to: number; reason: NoEvidenceReason }[] {
  const sorted = events
    .map((e) => ({
      ts: Date.parse(e.at),
      kind: e.kind,
      // Seconds on the wire (that is what the OS reports); milliseconds
      // everywhere in this file. Converted once, here, rather than at each
      // comparison — a unit mismatch against an epoch-ms instant is off by a
      // factor of 1000 and reads as "booted in 1970", which classifies every
      // outage as a reboot instead of failing visibly.
      bootTs: typeof e.bootTime === 'number' ? e.bootTime * 1000 : undefined,
      source: e.source,
    }))
    .filter((e) => !Number.isNaN(e.ts))
    .sort((a, b) => a.ts - b.ts);

  const out: { from: number; to: number; reason: NoEvidenceReason }[] = [];
  for (let i = 0; i < sorted.length; i++) {
    if (sorted[i].kind !== 'agent_offline') continue;
    // What may close an outage: only an instant whose EXISTENCE proves the
    // agent was running then.
    //
    // This used to close on the next event of any kind, reasoning that
    // "anything arriving from that host proves the agent was running again".
    // That is true of delivery, not of timestamps, and the swimlane is drawn
    // on timestamps — the reported case collapsed a fourteen-hour blackout
    // into ten seconds (#1245).
    //
    // Two things qualify, and they answer slightly different questions:
    //
    //  1. `agent_online` / `agent_offline` — the watchdog's own record of
    //     when it could hear the host. Authoritative about the CONNECTION,
    //     and the only thing that can attribute the outage, so this is what
    //     `outageReason` is given below. A later `agent_offline` closes this
    //     stretch too: the agent came back and dropped again, and the
    //     interval between the two is a separate matter.
    //
    //  2. any record from one of the AGENT's own producers (`source` of
    //     `agent:*` — the idle sampler, `agent:startup`). The agent wrote it
    //     while running, so its timestamp is an observation: at that instant
    //     the agent was alive and sampling, whenever the record happened to
    //     be delivered. This is the same evidence `outageReason` reads for
    //     the link-only case, applied to the stretch's extent rather than
    //     only to its label.
    //
    // Winlog stays excluded, which is the whole of #1245: the OS wrote those
    // records and the agent read them back out of the Event Log afterwards,
    // so a `sleep` stamped ten seconds after the agent died arrives fourteen
    // hours later and would collapse a fourteen-hour blackout into one. They
    // date the MACHINE, never the agent. An event with no `source` is not
    // counted either — it cannot be attributed, and guessing would put the
    // reassuring answer back on weak evidence.
    let next: { ts: number } | undefined;
    let backAt: number | undefined;
    for (let j = i + 1; j < sorted.length; j++) {
      if (sorted[j].ts <= sorted[i].ts) continue; // step over the same instant
      if (backAt === undefined && sorted[j].source?.startsWith('agent:') === true) {
        backAt = sorted[j].ts;
      }
      if (sorted[j].kind !== 'agent_online' && sorted[j].kind !== 'agent_offline') continue;
      next = sorted[j];
      break;
    }
    const from = Math.max(sorted[i].ts, t0);
    // The extent ends at the EARLIER proof; the reason is still derived from
    // the watchdog's own close. Keeping the two apart is what lets a link-only
    // outage hatch just the stretch before its first backfilled sample and
    // still say WHY that stretch is blank — narrowing the band and losing the
    // cause would trade one kind of dishonesty for another.
    const to = Math.min(backAt ?? next?.ts ?? liveEdge, t1);
    if (to > from) out.push({ from, to, reason: outageReason(sorted, i, next?.ts) });
  }
  return mergeRanges(out);
}

/**
 * Why the backend stopped hearing from the host — the question the hatch used
 * to answer with "not distinguishable" (#1316).
 *
 * Three causes look identical from the outside, because all three are
 * silence: the machine went away, the agent went away, or the link went away.
 * They are told apart by one fact the agent reports when it comes back, its
 * OS **boot time**, against one the backend already had, the last heartbeat
 * before the gap:
 *
 * | on return | boot vs. last beat | cause |
 * |---|---|---|
 * | `agent:startup` | after  | the host rebooted → the MACHINE was away |
 * | `agent:startup` | before | no reboot, so it was up → the AGENT was stopped |
 * | the AGENT's own events stamped INSIDE the gap | — | it was alive and observing → the LINK was away |
 *
 * The third row is positive evidence on purpose. "No startup event, so the
 * agent never restarted, so it was only the link" reads well and is unsound —
 * an agent too old to send a boot time still restarts, and one whose startup
 * event is still sitting in the outbox has not reported yet. Either would be
 * answered with the most reassuring of the three causes on the strength of
 * nothing. What actually identifies the link case is hearing FROM the host
 * about the silent stretch afterwards, which is the same late-delivery
 * property #1245 turns on.
 *
 * Deliberately no tolerance around the comparison. `boot_time` is derived
 * (`now − uptime`) and jitters by seconds within one boot session, but the
 * two instants being compared are minutes apart in every real case: a machine
 * that rebooted was off long enough to shut down and start, and one that did
 * not has a boot time from an earlier session entirely. A tolerance here
 * would only blur the one case it cannot help with.
 *
 * Falls back to the old undifferentiated `agentDown` whenever the startup
 * event carries no boot time — every agent older than this change, which is
 * most of a fleet until it rolls out. Saying "cannot tell" is the honest
 * answer there; guessing a cause from a missing field is not.
 */
function outageReason(
  sorted: { ts: number; kind: string; bootTs?: number; source?: string }[],
  offlineIdx: number,
  closedAt: number | undefined,
): NoEvidenceReason {
  const lastBeat = sorted[offlineIdx].ts;
  // The restart that ended THIS outage: an `agent_online` carrying a boot
  // time, at or after the close. Scanning from the close rather than from the
  // offline event matters — the watchdog's inferred `agent_online` and the
  // agent's own `agent:startup` describe the same recovery and can arrive in
  // either order, so the one with the payload is not necessarily the one that
  // closed the range.
  const startup = sorted.find(
    (e) =>
      e.source === 'agent:startup' &&
      e.bootTs !== undefined &&
      e.ts >= lastBeat &&
      (closedAt === undefined ||
        (e.ts <= closedAt + SAME_RECOVERY_MS &&
          // …and not the recovery of a LATER outage. The tolerance is ten
          // minutes and a flapping agent produces outages closer together
          // than that, so without this the second restart's boot time gets
          // compared against the FIRST outage's last heartbeat and attributes
          // a stretch it knows nothing about. An `agent_offline` between the
          // close and the startup opens a different silence; whatever comes
          // back after it belongs to that one.
          !sorted.some((o) => o.kind === 'agent_offline' && o.ts > closedAt && o.ts < e.ts))),
  );
  if (startup?.bootTs !== undefined) {
    return startup.bootTs > lastBeat ? 'agentDownHostOff' : 'agentDownAgentStopped';
  }
  // The link case needs POSITIVE evidence, not the absence of a restart.
  //
  // "No startup event, so the agent never restarted, so only the link was
  // down" reads well and is unsound: an agent too old to send a boot time
  // still restarts, and a startup event that is merely still in the outbox
  // has not arrived yet. Either would be reported as "the PC was fine" — the
  // most reassuring of the three answers, from no evidence at all.
  //
  // What does identify it is the agent's own records timestamped INSIDE the
  // stretch. The agent kept sampling while the backend could not hear it, and
  // those events arrive afterwards with their original `at` (the same
  // property #1245 turns on). Anything the HOST produced in there proves the
  // host and the agent were both alive, so what was missing was the link.
  //
  // "The host produced it" is narrower than "it is timestamped in there".
  // Winlog records are written by the OS and read out of the Event Log after
  // the agent returns, so a `sleep` stamped inside the gap proves the MACHINE
  // was on at that instant and says nothing about the agent — that is the
  // #1245 sequence exactly, where the agent was dead and the OS logged on
  // regardless. Only the agent's own producers (the idle sampler, `agent:*`)
  // are evidence that the AGENT was alive.
  //
  // An event with no `source` at all is not counted: it cannot be attributed,
  // and guessing here would put the reassuring answer back on weak evidence.
  const heardFromInside = sorted.some(
    (e) =>
      e.source?.startsWith('agent:') &&
      !OP_OBSERVATION_KIND_SET.has(e.kind) &&
      e.ts > lastBeat &&
      (closedAt === undefined || e.ts < closedAt),
  );
  return heardFromInside ? 'agentDownLinkOnly' : 'agentDown';
}

/**
 * How far after an outage closes an `agent:startup` may land and still count
 * as the same recovery.
 *
 * The watchdog closes the range from a heartbeat; the agent's own startup
 * event travels the obs outbox and can be a little behind. Generous, because
 * the alternative failure is worse: too tight and a real restart is read as
 * "link only", which is the one cause that claims nothing was wrong with the
 * host.
 */
const SAME_RECOVERY_MS = 10 * 60 * 1000;

/** Coalesce overlapping / touching ranges, keeping the first one's fields. */
function mergeRanges<T extends { from: number; to: number }>(ranges: T[]): T[] {
  const sorted = [...ranges].sort((a, b) => a.from - b.from);
  const out: T[] = [];
  for (const r of sorted) {
    const last = out[out.length - 1];
    if (last && r.from <= last.to) {
      if (r.to > last.to) last.to = r.to;
    } else {
      out.push({ ...r });
    }
  }
  return out;
}

/**
 * Stretches an unpaired `boot` proves nobody can account for.
 *
 * A machine cannot boot while it is already running. So a `boot` arriving
 * while the power lane believes the host is ON is proof of two things at
 * once: that it went down in between, and — because no END record sits
 * between the two — that nothing recorded WHEN.
 *
 * `buildSpans` cannot express that. Its rule for a start on an already-open
 * span is "ignore the duplicate start (a missed end shouldn't fragment the
 * interval)", which is right for `sleep` (modern standby logs several
 * transitions for one suspend) and for `logon`, and quietly paints the whole
 * dark stretch ON here: the evening's span simply swallows the next
 * morning's boot and runs on as if the machine never left.
 *
 * Both ways of getting here are ordinary:
 *   - an unclean power-off — power cut, hard reset, BSOD — records no
 *     `shutdown` and no `log_service_stopped` AT ALL. The only trace is
 *     Kernel-Power 41, and that is stamped at the next boot (see `marks` on
 *     the power lane), so it dates nothing;
 *   - a clean shutdown whose records fell outside the collector's 24 h
 *     backfill window, or behind a `limit`-truncated fetch.
 *
 * The stretch runs from the newest instant ANY event vouched for the host to
 * the boot itself. Any event: every kind's `at` is an instant the host
 * demonstrably existed at, including the watchdog's `agent_offline`, whose
 * `at` is the last heartbeat and is therefore usually the tightest bound
 * available. Before it the host was up and the span keeps painting; after the
 * boot it is up again; in between only the FACT of a power cycle is known.
 *
 * Which is why the answer is a hatch rather than a gap. Leaving it blank
 * would assert a measured OFF reaching back to whenever the host last
 * happened to log something — on a quiet desktop, hours before it actually
 * went down — and "blank means measured off" is the claim this component
 * exists to stop making without evidence (#1086).
 *
 * Only `boot` counts as the proof, deliberately:
 *   - `log_service_started` (6005) rides the same boot, so counting it would
 *     emit a second, sub-second stretch for one power cycle; and it means the
 *     event-log SERVICE started, which on its own does not say the machine
 *     had been off.
 *   - `unexpected_shutdown` (41) is excluded for the reason it is a marker at
 *     all — its timestamp is that same boot's, so it would date the outage
 *     from the moment the machine came back.
 *
 * And nothing the boot itself produced may BOUND the stretch either, which is
 * the same fact one step along. `boot` (12), `log_service_started` (6005) and
 * `unexpected_shutdown` (41) are three providers writing during one kernel
 * phase, and their order within that second is not fixed. Letting any of them
 * advance "the last instant something vouched for the host" means whichever
 * happened to sort first becomes the bound, and a twelve-hour dark night
 * collapses to the milliseconds between two records of the same boot — this
 * function's own bug, one line to the left.
 *
 * So the bound comes from the previous session only: the newest event that is
 * neither a power start nor a mark. When the session produced nothing at all
 * — a host whose whole history is boots — it falls back to the instant that
 * session began, which is still a real observation of the host being up and
 * keeps consecutive bare boots from merging into one stretch.
 */
export function unrecordedShutdownRanges(
  events: OpEvent[],
  t0: number,
  t1: number,
): { from: number; to: number }[] {
  const power = OP_LANES.find((l) => l.key === 'power')!;
  const starts: ReadonlySet<string> = new Set<string>(power.starts);
  const ends: ReadonlySet<string> = new Set<string>(power.ends);
  const marks: ReadonlySet<string> = new Set<string>(power.marks);
  const sorted = events
    .map((e) => ({ ts: Date.parse(e.at), kind: e.kind }))
    .filter((e) => !Number.isNaN(e.ts))
    .sort((a, b) => a.ts - b.ts);

  const out: { from: number; to: number }[] = [];
  // Mirrors `buildSpans`' own state machine, seeded OFF: only a START turns
  // the belief on, so a window whose power evidence begins with an END (the
  // carry-in case) never reports a phantom cycle at its left edge.
  let on = false;
  // Newest instant the CURRENT session produced, and the instant it began.
  let lastEvidence: number | undefined;
  let sessionStart: number | undefined;
  for (const e of sorted) {
    if (e.kind === 'boot' && on) {
      const bound = lastEvidence ?? sessionStart;
      if (bound !== undefined) {
        const from = Math.max(bound, t0);
        const to = Math.min(e.ts, t1);
        if (to > from) out.push({ from, to });
      }
      // …and this boot opens the session that follows it. Without the reset a
      // third bare boot would be bounded by the first and swallow the session
      // between them.
      sessionStart = e.ts;
      lastEvidence = undefined;
    } else if (starts.has(e.kind)) {
      // A start while already on is the same boot's second record (6005
      // beside 12): it opens nothing and must not become the bound.
      if (!on) {
        sessionStart = e.ts;
        lastEvidence = undefined;
      }
      on = true;
    } else if (ends.has(e.kind)) {
      lastEvidence = e.ts;
      on = false;
    } else if (!marks.has(e.kind)) {
      lastEvidence = e.ts;
    }
  }
  return mergeRanges(out);
}

/**
 * The stretches the strip must not make any claim about. Several causes, one
 * meaning — "no evidence either way":
 *
 *   [t0, coverEdge)              the fetch was truncated before reaching here
 *   [certainEdge, liveEdge]      the agent stopped reporting
 *   each `agent_offline` stretch the backend recorded hearing nothing
 *   each unpaired `boot`         the host power-cycled and nothing dated it
 *
 * Returned non-overlapping and in order. Overlap is not hypothetical: the
 * coverage floor is global across PCs, so a host that went quiet days ago
 * sits behind a floor set by a busier host's recent events and its
 * `certainEdge` lands before `coverEdge`; a never-reported agent with no
 * events puts `certainEdge` at 0 and overlaps outright; and a host that lost
 * power went quiet at the same moment, so its recorded outage and its
 * unpaired boot describe overlapping stretches by construction. Two entries
 * over the same pixels would stack two elements and leave which `reason` the
 * tooltip shows to paint order.
 */
export function noEvidenceRanges(
  t0: number,
  t1: number,
  now: number,
  coverEdge: number,
  lastHeartbeat?: string | null,
  lastEventTs?: number,
  // The host's events, so recorded `agent_offline` outages can be folded in
  // alongside the live-edge and truncation stretches.
  events: OpEvent[] = [],
): { from: number; to: number; reason: NoEvidenceReason }[] {
  const { certainEdge, liveEdge } = evidenceEdges(t1, now, lastHeartbeat, lastEventTs);

  // No operational events at all: every lane is empty, and an empty lane
  // means "measured, and it was off". Nothing here was measured. This is
  // reachable only for a strip that exists without events — a PC the
  // operator named explicitly — where rendering four blank lanes would state
  // the machine was off, signed out and idle for the whole window on the
  // strength of no data whatsoever. A heartbeat doesn't rescue it either: a
  // live agent proves the host is up *now*, not what any lane was doing
  // across a window it recorded no transitions in.
  //
  // Judged on LANE events only. Observation events (`agent_offline` /
  // `agent_online`) say when we were listening, never what any lane was
  // doing — a host with nothing but those still has four blank lanes and
  // still knows nothing about them. Counting them as evidence would put the
  // "four blank lanes asserting off" bug straight back for exactly the hosts
  // this feature exists to describe.
  const hasLaneEvidence = events.length
    ? events.some((e) => !OP_OBSERVATION_KIND_SET.has(e.kind))
    : lastEventTs !== undefined;
  if (!hasLaneEvidence) {
    return liveEdge > t0 ? [{ from: t0, to: liveEdge, reason: 'noEvents' }] : [];
  }

  const out: { from: number; to: number; reason: NoEvidenceReason }[] = [];
  if (coverEdge > t0) out.push({ from: t0, to: coverEdge, reason: 'truncated' });
  // Clamped past the truncated stretch, not merely past the window start.
  // `coverEdge >= t0` always, so this subsumes the window-start clamp.
  const from = Math.max(certainEdge, coverEdge);
  if (liveEdge > from) out.push({ from, to: liveEdge, reason: 'offline' });

  // Recorded outages, then the whole set is made disjoint. A recorded outage
  // can overlap either of the above — an agent that dropped and never came
  // back has both an `agent_offline` event and a stale live edge — and
  // overlapping ranges would stack elements and leave the displayed reason to
  // paint order. Earlier entries win, so the live-edge and truncation
  // stretches keep their more specific reasons where they coincide.
  const recorded = agentDownRanges(events, t0, t1, liveEdge);
  for (const r of recorded) {
    for (const piece of subtractRanges([r], out)) out.push(piece);
  }

  // Power cycles nothing recorded the start of. Folded in LAST so the causes
  // above keep their more specific reason wherever they coincide: a host that
  // lost power stopped heartbeating at that instant, so the same stretch is
  // usually also a recorded outage, and "the agent stopped reporting" is the
  // stronger statement — it is dated by a heartbeat rather than bounded by
  // whatever the host last happened to log.
  for (const r of unrecordedShutdownRanges(events, t0, t1)) {
    const band = { ...r, reason: 'unrecordedShutdown' as const };
    for (const piece of subtractRanges([band], out)) out.push(piece);
  }
  return out.sort((a, b) => a.from - b.from);
}

/**
 * The unknown band, narrowed to what each lane actually doesn't know.
 *
 * `noEvidenceRanges` answers one question for the whole strip — "when were we
 * not listening" — and the draw site erases it with that lane's OWN spans.
 * Nothing carries evidence sideways, so a lane stays blind to what the lanes
 * above it recorded about the same instant. The strip then asserts two things
 * at once over the same pixels: a laptop suspended from 18:06 to 08:03 has a
 * sleep span saying exactly that, while `active` and `session` hatch the whole
 * night as unknown (#1322). It cannot be both.
 *
 * The rule is not "trust the other lanes" in general. It is narrower, and it
 * is physical: **while the machine is suspended or off, no state can change**.
 * Whatever a subordinate lane was doing when the machine went into that
 * interval is what it was doing when it came out, and nobody had to be
 * listening to know it. Sleep and power-off are the two intervals the OS
 * records both ends of, which is what makes them usable here — the same
 * property that decides the outage cut in #1245.
 *
 * No lane is settled by the lane ABOVE it, `power` least of all — it is the
 * one the others are subordinate to. What settles a lane is either its own
 * records or the physical rule above:
 *   `power`   — its own recorded off intervals. A `shutdown` and the next
 *               `boot` bound a stretch the OS accounted for at both ends, so
 *               "the machine was off" is measured there, not guessed. The
 *               agent is of course silent across it — that is what being off
 *               means — and hatching the night of every cleanly shut down
 *               desktop while holding the two records that explain it is
 *               #1322 one lane up.
 *   `sleep`   — power-off alone: a machine that is off is not asleep.
 *   `session`
 *   `active`  — either.
 *
 * Which intervals settle anything comes from `recordedIntervals`, i.e. from
 * the RECORDS, never from the rendered spans. A span's edges are a drawing
 * decision as much as an observation — `buildSpans` carries in at both window
 * edges, `clipToOn` moves an edge onto a power boundary, the heartbeat gate
 * cuts one — and a span reaching an instant is not the same claim as the OS
 * having logged both ends around it. The #1245 sequence is exactly that
 * difference: one end recorded, hours of nothing after it, and a right edge
 * the renderer supplied.
 *
 * Reading the span flags instead does not work, which is worth writing down
 * because it looks like it should. `clipToOn` recomputes `openStart` as
 * `s.openStart && from === s.from`, so a carried-in span whose left edge is
 * clipped to a `boot` comes back with `openStart: false` — laundered into
 * looking recorded. A bare `resume` with no `sleep` in the window then reads
 * as "suspended ever since the boot", which is false: the machine was up, and
 * the `sleep` record is missing or still in flight.
 *
 * "Off" likewise means asserted off. Reading the complement of the power lane
 * would let a stretch we know nothing about silence the lanes below it, which
 * is #1086's asymmetry one level down.
 *
 * Returns one entry per input lane, in the same order.
 */
export function noEvidenceByLane<
  L extends { key: LaneKey; spans: { from: number; to: number }[] },
  N extends { from: number; to: number },
>(lanes: L[], bands: N[], recorded: RecordedIntervals): N[][] {
  const offOrAsleep = mergeRanges(
    [...recorded.off, ...recorded.asleep].map((r) => ({ from: r.from, to: r.to })),
  );
  return lanes.map((lane) => {
    // `power` is settled by its own recorded off intervals; `sleep` by
    // power-off alone (a machine that is off is not asleep); the rest by
    // either.
    const settled =
      lane.key === 'power' || lane.key === 'sleep' ? recorded.off : offOrAsleep;
    return subtractRanges(bands, [...lane.spans, ...settled]);
  });
}

export type RecordedIntervals = {
  /** Powered off: a power END record, then the next power START record. */
  off: { from: number; to: number }[];
  /** Suspended: a `sleep` record, then the next `resume` record. */
  asleep: { from: number; to: number }[];
};

/**
 * The intervals the OS accounted for at BOTH ENDS, taken from the events.
 *
 * Deliberately not derived from the lanes. Spans exist to be drawn, so their
 * edges carry the renderer's decisions, and none of those are the OS saying
 * anything. Pairing the raw records keeps "the machine was suspended across
 * here" a claim the event log made — the only kind strong enough to settle a
 * neighbouring lane.
 *
 * An open with no close yields NOTHING. A `resume` with no `sleep` before it
 * does prove the machine had been suspended, but not since when: the missing
 * `sleep` may be a record still in flight, and the stretch ahead of it is
 * exactly the part nobody can account for.
 *
 * Records OUTSIDE the window still pair, and the result is clamped to it. A
 * `sleep` the seed lookup found an hour before `t0` is a record like any
 * other; refusing it would hatch the start of the window as unknown while
 * holding the very log line that accounts for it — this fix's own bug, one
 * step to the left. What the clamp drops is the part outside the window,
 * which was never going to be drawn.
 *
 * But not ACROSS A COVERAGE HOLE. `[t0, coverEdge)` is the stretch a `limit`
 * fetch never returned, and a pair reaching over it cannot rule out an
 * intervening cycle whose records are simply absent. This is #1326's rule and
 * the distinction it turns on: an agent outage preserves coverage, because
 * the OS keeps writing to the event log and the collector backfills on
 * return, so pairs may bridge one; a truncation loses records outright, so
 * they may not.
 *
 * Measured before it was written: on the #1326 shape — a `sleep` the night
 * before, its morning `resume` truncated away, the next one a day later —
 * this produced a twenty-hour "suspend" and erased thirteen hours of
 * legitimate agent-down hatch. #1326 fixed the span; without this the same
 * artefact came back through the settling intervals.
 */
export function recordedIntervals(
  events: OpEvent[],
  t0: number,
  t1: number,
  // Oldest instant the event set actually covers; `t0` when nothing was
  // truncated, which is the case that changes nothing here.
  coverEdge: number = t0,
): RecordedIntervals {
  const sorted = events
    .map((e) => ({ ts: Date.parse(e.at), kind: e.kind }))
    .filter((e) => !Number.isNaN(e.ts))
    .sort((a, b) => a.ts - b.ts);
  // A second open before the close replaces the first rather than closing it:
  // two `sleep` records in a row mean the first pairing never happened, and
  // pairing across it would invent one interval spanning both.
  const pairs = (opens: Set<string>, closes: Set<string>) => {
    const out: { from: number; to: number }[] = [];
    let open: number | undefined;
    for (const e of sorted) {
      if (open !== undefined && closes.has(e.kind)) {
        // Opened before the data starts: the record that would have closed it
        // may be inside the hole, so this close is not necessarily its match.
        const spansHole = coverEdge > t0 && open < coverEdge;
        const [from, to] = [Math.max(open, t0), Math.min(e.ts, t1)];
        if (!spansHole && to > from) out.push({ from, to });
        open = undefined;
      } else if (opens.has(e.kind)) {
        open = e.ts;
      }
    }
    // Whatever is still open at the end stays unpaired on purpose: the close
    // is what makes the interval the OS's claim rather than ours.
    return out;
  };
  const power = OP_LANES.find((l) => l.key === 'power')!;
  const sleep = OP_LANES.find((l) => l.key === 'sleep')!;
  return {
    // Powered-off runs the pairing backwards: the lane's END kinds open the
    // off-interval and its START kinds close it.
    off: pairs(new Set<string>(power.ends), new Set<string>(power.starts)),
    asleep: pairs(new Set<string>(sleep.starts), new Set<string>(sleep.ends)),
  };
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
/**
 * Cut a sleep span where the host proves it was not asleep.
 *
 * A suspended machine records nothing. So any event of the host's OWN making
 * timestamped strictly inside a sleep span contradicts that span outright —
 * an `active` sample, a `logoff`, a `boot`: whatever produced it, the machine
 * was running at that instant.
 *
 * The span reaches there in the first place because `buildSpans` carries in:
 * when the first sleep-lane record it can see is a `resume`, it reasons that
 * the state must already have been open, which is normally sound — a resume
 * does imply a prior sleep. It stops being sound when the record that closed
 * the EARLIER sleep is missing, and `limit` truncation loses records outright
 * (unlike an agent outage, where the OS keeps writing and the collector
 * backfills on return — the distinction #1245 turns on). The observed case:
 * a laptop suspended the night before, its morning `resume` dropped by a
 * `limit=200` fetch, and the carried-in span paired with the FOLLOWING day's
 * resume — four hours of "asleep" drawn over a stretch the same strip painted
 * as interactive, with a `logoff` marker sitting in the middle of it (#1326).
 *
 * Two kinds of event are excluded, for different reasons.
 *
 * Observation kinds are not the host's making. `agent_offline` is written by
 * the backend's heartbeat watchdog precisely BECAUSE the machine went quiet,
 * so it is the one thing that appears during a genuine suspend; counting it
 * would cut every real overnight sleep at the moment the agent dropped.
 *
 * The lane's OWN kinds are excluded because `buildSpans` already decided what
 * a repeated `sleep` means: "a second start while already open is ignored (a
 * missed end shouldn't fragment the interval)". Counting it here would undo
 * that silently — an eight-hour suspend with one duplicate record half an
 * hour in came out as a thirty-minute span, the rest of the night dropped.
 * Duplicates are not hypothetical: modern standby can log more than one
 * sleep-kind transition for what the user experienced as a single suspend,
 * and delivery is at-least-once.
 *
 * That tolerance is a genuine trade — a second `sleep` could equally mean the
 * host woke and re-slept with the `resume` lost, in which case cutting would
 * be right. It cannot be told apart from the records, so the two places that
 * face the question answer it the same way rather than each guessing.
 * (`resume` never lands strictly inside a span of its own lane anyway, since
 * `buildSpans` closes on the first one it meets.)
 */
const sleepKinds = OP_LANES.find((l) => l.key === 'sleep')!;

function cutAtContradiction(spans: Span[], events: OpEvent[]): Span[] {
  const own = new Set<string>([...sleepKinds.starts, ...sleepKinds.ends]);
  const stamps = events
    .filter((e) => !OP_OBSERVATION_KIND_SET.has(e.kind) && !own.has(e.kind))
    .map((e) => Date.parse(e.at))
    .filter((ts) => !Number.isNaN(ts))
    .sort((a, b) => a - b);
  if (!stamps.length) return spans;
  return spans
    .map((s) => {
      // Strictly inside: the span's own boundary records sit at its edges.
      const hit = stamps.find((ts) => ts > s.from && ts < s.to);
      return hit === undefined ? s : { ...s, to: hit, openEnd: false, cutByHeartbeat: true };
    })
    .filter((s) => s.to > s.from);
}

/**
 * Take the unaccountable stretches back out of the power lane.
 *
 * `buildSpans` swallowed the second `boot` as a duplicate start, so one span
 * now covers both power-on sessions AND the dark stretch between them.
 * Subtracting restores the two sessions and leaves the middle to the
 * no-evidence band, which folds the identical ranges in (`noEvidenceRanges`)
 * so the hatch lands exactly where the paint was removed.
 *
 * Per source span, so the edge flags can be repaired against it:
 * `subtractRanges` copies them onto BOTH pieces, so a split would otherwise
 * leave the left piece claiming `openEnd` ("still running at the window
 * edge") and the right claiming `openStart` ("continued from before this
 * period"). Both are false of a piece whose edge is the power cycle. Same
 * repair `clipToOn` and the suspend subtraction do.
 */
function cutAtUnrecordedShutdown(spans: Span[], cuts: { from: number; to: number }[]): Span[] {
  if (!cuts.length) return spans;
  return spans.flatMap((sp) =>
    subtractRanges([sp], cuts).map((piece) => ({
      ...piece,
      openStart: piece.openStart && piece.from === sp.from,
      openEnd: piece.openEnd && piece.to === sp.to,
      // A cut edge abuts the hatch that replaces it, so it must be square —
      // the same rendering requirement `cutByHeartbeat` already carries.
      cutByHeartbeat: piece.cutByHeartbeat || piece.to !== sp.to,
    })),
  );
}

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
  const powerSpans = cutAtUnrecordedShutdown(
    buildSpans(events, power.starts, power.ends, t0, t1, now),
    unrecordedShutdownRanges(events, t0, t1),
  );
  const onIntervals = powerSpans.map((s) => ({ from: s.from, to: s.to }));
  // Only clip when we actually have power events — a PC that reports just
  // active/idle (no winlog power lane) has no ON spans, and clipping to an
  // empty set would erase its only signal.
  const powerKinds = new Set<string>([...power.starts, ...power.ends]);
  // Kind membership is still how the FALLBACK works when a caller supplies no
  // `source` (see below), and it carries the old hazard: a truncated fetch
  // that drops a host's power events while keeping its session/sampler
  // traffic makes the fallback answer "no winlog". Callers that pass `source`
  // are immune, and both surfaces do — the Events page reads it off
  // `/api/obs_events`, the Analytics widget off the `op_timeline` payload.
  // #1256: this used to be `events.some((e) => powerKinds.has(e.kind))`,
  // which reads as "does this host run the winlog collector?" but measures
  // "did this host reboot inside the window?". Those diverge on any host that
  // stays up longer than the window — the common case for a laptop that
  // suspends at night — and when they diverge the lanes below get
  // re-synthesised from the sampler envelope while the sleep lane, drawn from
  // the very winlog events the predicate just declared absent, contradicts
  // them on the same strip.
  //
  // `source` answers the intended question directly. It is optional on the
  // type, so fall back to the old kind test when a caller has none: wrong in
  // the same way as before rather than newly wrong.
  const hasWinlog = events.some((e) =>
    e.source === undefined ? powerKinds.has(e.kind) : e.source.startsWith('winlog:'),
  );
  // Whether clipping to power is MEANINGFUL, which is a different question
  // from whether winlog exists. Decoupling `hasWinlog` from the power kinds
  // (#1256) separated the two: a host can report winlog session/sleep events
  // and still have no power span in the window — a freshly onboarded host
  // whose real boot predates retention, or one the seed lookup found no
  // power event for. `clipToOn(spans, [])` returns nothing, so clipping then
  // erases the very evidence the lane exists to show. This is the guarantee
  // the old `hasPower` gate happened to provide for free, and it has to be
  // stated separately now that the predicate no longer implies it.
  const canClipToPower = onIntervals.length > 0;
  // Recorded outages, used to stop an UNCLOSED span from bridging them.
  //
  // Not to erase closed spans, and not scoped to one lane. Every producer —
  // winlog, the idle sampler, the self-update reporter — writes to the same
  // `obs_outbox` and the drain publishes when it can, so ANY event may arrive
  // late with its original timestamp. A NATS outage therefore does not mean
  // the agent stopped observing; it means the backend stopped hearing. Cutting
  // real spans on that signal would throw away evidence that is merely in
  // flight.
  //
  // What is safe to act on is the ABSENCE of a closing event. `openEnd` means
  // no matching end was found, so `buildSpans` ran the span to the window
  // edge. That is the #1245 shape: the sampler debounces over five minutes and
  // the machine suspends ten seconds after logoff, so the closing `idle` is
  // never generated — not delayed, never produced — and an `active` opened in
  // the evening painted through to the next morning's first idle.
  //
  // The two cases separate cleanly on this test:
  //   - connectivity only: the closing event exists and arrives late, the span
  //     is closed, nothing is truncated;
  //   - host gone: no closing event exists, the span is open, and it ends
  //     where the agent went quiet.
  // Neither depends on knowing WHY the agent went quiet, which is just as
  // well — the heartbeat watchdog cannot tell a dead host from a dead link.
  const outages = agentDownRanges(events, t0, t1, liveEdge);
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
  // Sleep is built ahead of the map because the active lane needs it: a
  // suspended machine cannot be typed on, so a sleep span bounds `active`
  // the way a power span does. Same pipeline the generic branch would run.
  const sleepLane = OP_LANES.find((l) => l.key === 'sleep')!;
  let sleepSpans = buildSpans(events, sleepLane.starts, sleepLane.ends, t0, t1, now);
  if (hasWinlog && canClipToPower) sleepSpans = clipToOn(sleepSpans, onIntervals);
  sleepSpans = cutAtContradiction(sleepSpans, events);
  return OP_LANES.map((lane) => {
    let spans: Span[];
    // Whether this lane's spans rest on the SAMPLER'S SILENCE, which is what
    // decides whether the outage cut below applies to them. See the comment at
    // the cut itself for why the two kinds of span are not interchangeable.
    let restsOnSilence: boolean;
    if (lane.key === 'power') {
      spans = hasWinlog ? powerSpans : presenceSpans;
      restsOnSilence = !hasWinlog;
    } else if (lane.key === 'session') {
      // Both session paths in one branch so the generic branch below doesn't
      // rebuild the spans `sessionSpans` already holds.
      spans = hasWinlog
        ? canClipToPower
          ? clipToOn(sessionSpans, onIntervals)
          : sessionSpans
        : mergeSpans([...sessionSpans, ...presenceSpans]);
      restsOnSilence = !hasWinlog;
    } else if (lane.key === 'active') {
      // Reuse the pre-built active spans; still clip to power ON when winlog
      // exists so a stale open span can't paint across a powered-off gap.
      spans = hasWinlog && canClipToPower ? clipToOn(activeSpans, onIntervals) : activeSpans;
      // …and out of the stretches the host spent suspended. The two lanes
      // used to be free to contradict each other: the suite related each to
      // `power` and never to the other, so "asleep AND interactive at the
      // same instant" broke no rule and was drawn (#1326).
      //
      // Which one gives way is not arbitrary — RECORDS BEAT SPANS. An
      // `active` sample is a record and proves the machine was awake, which
      // is why it cuts a sleep span in `cutAtContradiction` above. But an
      // `active` span reaching into a suspend is not a record: it is an open
      // interval the sampler never closed, because the machine went away
      // before the five-minute debounce could fire (#1245's mechanism, minus
      // the recorded outage that lets #1245 cut it). Between a reconstruction
      // and the OS's own `sleep`, the OS wins.
      //
      // Applied after the outage cut below, not here: subtracting first
      // SPLITS the span, and the cut only truncates a span the outage starts
      // inside — so the far piece would slip past it and paint the night
      // again. #1245 caught by its own test.
      restsOnSilence = true;
    } else {
      // Sleep, and only sleep — the other three are handled above, so this
      // is the whole of `OP_LANES` and the compiler knows it.
      spans = sleepSpans;
      restsOnSilence = false;
    }
    // A state open when the agent died is not observed across the outage.
    // `subtractRanges` copies the source span's fields onto both pieces, so
    // repair the edge flags: a piece whose start moved did not begin before
    // the window, and one whose end moved is not still open at its end.
    // Don't let a span BRIDGE the moment the agent went quiet.
    //
    // Not a subtraction of the outage: a network-only outage still has the
    // agent sampling, so genuine short spans lie wholly INSIDE it and
    // subtracting would erase them. Only the span that was already open when
    // the agent went quiet is cut, and only at that instant; anything
    // starting later paints normally.
    //
    // This is the #1245 shape. The evening's `active` was paired with the
    // NEXT MORNING's first `idle` — the reconstruction has no other candidate
    // — so the span was "closed" and looked healthy while claiming fourteen
    // hours of work across a night the machine spent suspended. What it
    // actually bridges is a stretch where nothing was heard, and a span that
    // spans silence is not evidence of the state it asserts.
    //
    // Only the spans that rest on that silence, though. Structurally an
    // `active`→`idle` pair fourteen hours apart and a `sleep`→`resume` pair
    // fourteen hours apart look identical, but they claim their interval on
    // different grounds:
    //
    //   - `sleep`→`resume`, `boot`→`shutdown`, `logon`→`logoff`: the OS
    //     recorded BOTH ENDS, and the pair itself asserts the interval between
    //     them. The kernel logged the resume; the machine could not have left
    //     that state without an event. Late delivery does not weaken it — the
    //     record still describes the night no matter when it arrived.
    //   - `active`→`idle`: the sampler never says "still active"; it says
    //     "changed". The claim on the interval is the ABSENCE of a transition,
    //     and absence is exactly what a dead agent produces. So it is not
    //     evidence for the stretch where the agent was not heard from.
    //
    // Cutting the first kind was the over-correction: it threw away genuine,
    // OS-recorded sleep and power evidence to fix a sampler artefact. The lane
    // backfill (#970) inherits the sampler's grounds along with its spans, so
    // `restsOnSilence` follows the source of the spans rather than the lane.
    //
    // But "the OS recorded both ends" is a property of a SPAN, not of a lane,
    // and an `openEnd` span is precisely the one where it does not hold: no
    // closing record was found, so `buildSpans` ran the state to the window
    // edge. Its tail is the renderer's, not the kernel's. That is the shape an
    // unclean power-off leaves — nothing is logged at the time, so the
    // evening's `boot` span runs on claiming the host is up right now while
    // the watchdog has heard nothing since yesterday evening.
    //
    // Only against an outage that is STILL OPEN at the span's end, though.
    // A network blip that closed is proof of the opposite: the agent came
    // back, and with no `boot` between the two the host never went down, so
    // the span is right to run through it. Cutting on any outage would leave
    // every host that ever lost its link showing as off from that moment on.
    // A power cycle inside a closed outage is not this rule's business
    // either — `unrecordedShutdownRanges` has the `boot` that proves it.
    if (outages.length) {
      spans = spans
        .map((sp) => {
          const o = outages.find(
            (x) =>
              x.from > sp.from &&
              x.from < sp.to &&
              (restsOnSilence || (sp.openEnd && x.to >= sp.to)),
          );
          // `cutByHeartbeat` is the existing "hard cut, abuts a hatch" edge
          // style; the reason differs but the rendering requirement is the
          // same, and `openEnd` must clear so the tooltip stops claiming the
          // state runs off the window edge.
          return o ? { ...sp, to: o.from, openEnd: false, cutByHeartbeat: true } : sp;
        })
        .filter((sp) => sp.to > sp.from);
    }
    // Out of the stretches the OS recorded as suspended — see the note in the
    // active branch for why the sleep span wins over an unclosed active one,
    // and why this runs here rather than there.
    if (lane.key === 'active') {
      // Per source span, so the edge flags can be repaired against it:
      // `subtractRanges` copies them onto BOTH pieces, so a span split by a
      // suspend would leave a left piece still claiming `openEnd` ("running
      // past the window edge") and a right piece claiming `openStart`
      // ("continued from before this period"). Both are false of a piece
      // whose edge is the sleep boundary. Same repair `clipToOn` does.
      spans = spans.flatMap((sp) =>
        subtractRanges([sp], sleepSpans).map((piece) => ({
          ...piece,
          openStart: piece.openStart && piece.from === sp.from,
          openEnd: piece.openEnd && piece.to === sp.to,
        })),
      );
    }
    // Hatch any ongoing state past the agent's last heartbeat as unconfirmed.
    spans = gateToHeartbeat(spans, certainEdge, liveEdge);
    return {
      key: lane.key as LaneKey,
      color: lane.color,
      spans,
      markers: laneMarkers(events, [...lane.starts, ...lane.ends, ...lane.marks], t0, t1),
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
    () => noEvidenceRanges(t0, t1, now, coverEdge, lastHeartbeat, newestEventTs(events), events),
    [events, t0, t1, now, lastHeartbeat, coverEdge],
  );

  // …narrowed per lane, because the band above is one answer for the whole
  // strip and a lane's own spans are not the only thing that can settle it.
  // Both readers use this: drawing it on one basis and reporting it on
  // another would let the crosshair say "unknown" over pixels that aren't
  // hatched. Indexed positionally — `noEvidenceByLane` returns one entry per
  // input lane, in order.
  const recorded = useMemo(
    () => recordedIntervals(events, t0, t1, coverEdge),
    [events, t0, t1, coverEdge],
  );
  const laneBands = useMemo(
    () => noEvidenceByLane(lanes, noEvidence, recorded),
    [lanes, noEvidence, recorded],
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
      : lanes.map((lane, i) => {
          const s = lane.spans.find((x) => x.from <= hoverTs && hoverTs < x.to);
          // No span AND inside a no-evidence stretch → unknown, not "off".
          // Reporting "off" here is the exact failure this change exists to
          // remove: an absent span is only meaningful where we were listening.
          // Against THIS lane's band: a machine asleep all night settles
          // `active` without painting it, so the readout must say "off" there
          // rather than "unknown" — matching the pixels beside it.
          const blind =
            !s && laneBands[i].some((n) => n.from <= hoverTs && hoverTs < n.to);
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
      {lanes.map((lane, laneIdx) => (
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
            {laneBands[laneIdx].map((n, i) => (
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
