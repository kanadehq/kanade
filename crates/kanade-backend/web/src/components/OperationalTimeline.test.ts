import { describe, expect, test } from 'bun:test';
import { AGENT_ACTIVE_THRESHOLD_MS } from '@/lib/utils';

import { axisGridlines, axisTicks, buildLanes } from './OperationalTimeline';

// The lane rules are a small set of interacting invariants that have been
// fixed four separate times (#841, #972, #981, #983, and the session backfill
// gate), always from a screenshot rather than a failing test. This file
// pins them down as a matrix: for each combination of "which feeds does this
// PC actually report", assert what every lane is allowed to show.

// A fixed one-day window, so the expectations can be written in plain hours.
const T0 = Date.parse('2026-07-19T00:00:00Z');
const T1 = Date.parse('2026-07-20T00:00:00Z');
const NOW = T1; // the window ends at the present; nothing is clamped early

/** Hours after T0, as an ISO instant. */
const h = (n: number) => new Date(T0 + n * 3_600_000).toISOString();
/** Hours after T0, as an epoch ms — for comparing against span bounds. */
const H = (n: number) => T0 + n * 3_600_000;

const ev = (at: string, kind: string) => ({ at, kind });

type Lane = ReturnType<typeof buildLanes>[number];
const lane = (lanes: Lane[], key: string): Lane => {
  const l = lanes.find((x) => x.key === key);
  if (!l) throw new Error(`no ${key} lane`);
  return l;
};

/** Total painted time on a lane, in hours. Spans never overlap. */
const covered = (l: Lane) => l.spans.reduce((sum, s) => sum + (s.to - s.from), 0) / 3_600_000;

/** Does the lane paint the whole instant `ts`? */
const coversAt = (l: Lane, ts: number) => l.spans.some((s) => s.from <= ts && ts < s.to);

/** Is every span of `sub` inside some span of `sup`? The lane containment rule. */
const containedIn = (sub: Lane, sup: Lane) =>
  sub.spans.every((s) => sup.spans.some((o) => o.from <= s.from && o.to >= s.to));

// ---------------------------------------------------------------------------
// The matrix: which feeds a PC reports -> what each lane must show.
// ---------------------------------------------------------------------------

describe('lane matrix: which feeds the PC reports', () => {
  // Case 1 — the fleet-normal host: winlog power + session, plus the sampler.
  test('full winlog + sampler: every lane comes from its own events', () => {
    const lanes = buildLanes(
      [
        ev(h(1), 'boot'),
        ev(h(2), 'logon'),
        ev(h(2), 'active'),
        ev(h(5), 'idle'),
        ev(h(6), 'sleep'),
        ev(h(7), 'resume'),
        ev(h(9), 'logoff'),
        ev(h(10), 'shutdown'),
      ],
      T0,
      T1,
      NOW,
    );

    expect(covered(lane(lanes, 'power'))).toBe(9); // boot 1h -> shutdown 10h
    expect(covered(lane(lanes, 'session'))).toBe(7); // logon 2h -> logoff 9h
    expect(covered(lane(lanes, 'sleep'))).toBe(1); // sleep 6h -> resume 7h
    expect(covered(lane(lanes, 'active'))).toBe(3); // active 2h -> idle 5h

    // Nothing is painted after the host powered off.
    expect(coversAt(lane(lanes, 'power'), H(11))).toBe(false);
    expect(coversAt(lane(lanes, 'session'), H(11))).toBe(false);
  });

  // Case 2 — the #970 / #981 host: no winlog at all, sampler only (minipc).
  test('sampler only: power and session are backfilled from the presence envelope', () => {
    const lanes = buildLanes(
      [ev(h(2), 'active'), ev(h(4), 'idle'), ev(h(8), 'active'), ev(h(10), 'idle')],
      T0,
      T1,
      NOW,
    );

    // #981: the envelope spans the idle stretch too — an `idle` event still
    // proves the sampler was running, i.e. the host was up and signed in.
    // Painting these from the active spans made them drop out every idle gap.
    expect(coversAt(lane(lanes, 'power'), H(6))).toBe(true);
    expect(coversAt(lane(lanes, 'session'), H(6))).toBe(true);
    expect(coversAt(lane(lanes, 'active'), H(6))).toBe(false); // the lane itself still shows the gap

    // #983: carry-in at both edges, so the envelope reaches the window edge
    // rather than starting at the first raw event.
    expect(coversAt(lane(lanes, 'power'), H(0.5))).toBe(true);
    expect(coversAt(lane(lanes, 'session'), H(0.5))).toBe(true);

    // Sleep is never inferred — the envelope says nothing about sleeping.
    expect(lane(lanes, 'sleep').spans).toHaveLength(0);
  });

  // Case 3 — the bug in this change: no winlog power, but ONE logon/logoff.
  test('sampler + a single session pair: the pair does not disable the backfill', () => {
    const events = [
      ev(h(2), 'active'),
      ev(h(4), 'idle'),
      ev(h(8), 'active'),
      ev(h(10), 'idle'),
      ev(h(6), 'logon'),
      ev(h(7), 'logoff'),
    ];
    const lanes = buildLanes(events, T0, T1, NOW);

    // Previously `hasSession` flipped on that one logon and the whole-window
    // backfill switched off, leaving a 1h span on an otherwise blank lane
    // while power and active were filled end to end.
    expect(covered(lane(lanes, 'session'))).toBeGreaterThan(1);
    expect(coversAt(lane(lanes, 'session'), H(0.5))).toBe(true);
    expect(coversAt(lane(lanes, 'session'), H(23))).toBe(true);

    // The genuine events still show as markers regardless of the spans.
    const kinds = lane(lanes, 'session').markers.map((m) => m.kind);
    expect(kinds).toContain('logon');
    expect(kinds).toContain('logoff');
  });

  // Case 4 — partial winlog: power events but no session feed.
  test('winlog power without a session feed: session stays empty, not backfilled', () => {
    const lanes = buildLanes(
      [ev(h(1), 'boot'), ev(h(2), 'active'), ev(h(5), 'idle'), ev(h(10), 'shutdown')],
      T0,
      T1,
      NOW,
    );

    // With real power events, winlog is authoritative — the sampler must not
    // union its way into lanes winlog is actually covering (#841).
    expect(covered(lane(lanes, 'power'))).toBe(9);
    expect(lane(lanes, 'session').spans).toHaveLength(0);
  });

  // Case 5 — session events but nothing else: no power feed, no sampler, so
  // the envelope is empty and the genuine spans are all there is. Pins the
  // backfill as a *union*: replacing the genuine spans with the envelope
  // would blank the only lane this host populates.
  test('session events with no power feed and no sampler: the genuine spans survive', () => {
    const lanes = buildLanes([ev(h(2), 'logon'), ev(h(9), 'logoff')], T0, T1, NOW);

    expect(covered(lane(lanes, 'session'))).toBe(7);
    expect(coversAt(lane(lanes, 'session'), H(5))).toBe(true);
    expect(lane(lanes, 'power').spans).toHaveLength(0);
    expect(lane(lanes, 'active').spans).toHaveLength(0);
  });

  test('no events at all: every lane is empty', () => {
    const lanes = buildLanes([], T0, T1, NOW);
    for (const l of lanes) expect(l.spans).toHaveLength(0);
  });

  test('a degenerate window yields no lanes', () => {
    expect(buildLanes([ev(h(1), 'boot')], T1, T0, NOW)).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// The invariants themselves — the properties every case above must satisfy.
// ---------------------------------------------------------------------------

describe('lane invariants', () => {
  // The rule #972/#981/#983 kept re-breaking: if the host was doing anything,
  // it was necessarily powered on and someone was signed in.
  test('active implies power and session, in every feed combination', () => {
    const cases: { name: string; events: { at: string; kind: string }[] }[] = [
      {
        name: 'full winlog',
        events: [
          ev(h(1), 'boot'),
          ev(h(2), 'logon'),
          ev(h(3), 'active'),
          ev(h(5), 'idle'),
          ev(h(9), 'logoff'),
          ev(h(10), 'shutdown'),
        ],
      },
      {
        name: 'sampler only',
        events: [ev(h(2), 'active'), ev(h(4), 'idle'), ev(h(8), 'active'), ev(h(10), 'idle')],
      },
      {
        name: 'sampler + one session pair',
        events: [
          ev(h(2), 'active'),
          ev(h(4), 'idle'),
          ev(h(6), 'logon'),
          ev(h(7), 'logoff'),
          ev(h(8), 'active'),
          ev(h(10), 'idle'),
        ],
      },
      {
        name: 'sampler with no closing idle',
        events: [ev(h(2), 'active')],
      },
      {
        name: 'idle before any active (carry-in)',
        events: [ev(h(3), 'idle'), ev(h(6), 'active'), ev(h(8), 'idle')],
      },
    ];

    for (const c of cases) {
      const lanes = buildLanes(c.events, T0, T1, NOW);
      expect(
        containedIn(lane(lanes, 'active'), lane(lanes, 'power')),
        `${c.name}: active must be inside power`,
      ).toBe(true);
      expect(
        containedIn(lane(lanes, 'active'), lane(lanes, 'session')),
        `${c.name}: active must be inside session`,
      ).toBe(true);
    }
  });

  // #841: a stale open state carried across a power cycle must not paint over
  // the powered-off gap.
  test('subordinate lanes never paint while power is off', () => {
    const lanes = buildLanes(
      [
        ev(h(1), 'boot'),
        ev(h(2), 'active'), // never closed by an `idle` — the sampler died with the host
        ev(h(3), 'shutdown'),
        ev(h(8), 'boot'),
        ev(h(12), 'shutdown'),
      ],
      T0,
      T1,
      NOW,
    );

    // 3h..8h the host was off.
    for (const key of ['session', 'sleep', 'active']) {
      expect(coversAt(lane(lanes, key), H(5)), `${key} must not paint while off`).toBe(false);
    }
    expect(coversAt(lane(lanes, 'power'), H(5))).toBe(false);
    expect(coversAt(lane(lanes, 'active'), H(2.5))).toBe(true); // but it does paint while on
  });

  // The active lane has its own clip; sleep goes through the generic branch,
  // so it needs its own case — a sleep left open across a power cycle (the
  // host slept, then lost power) must be cut at the shutdown, not painted
  // straight through the dark stretch to the eventual resume.
  test('a sleep span open across a power cycle is clipped at both ends of the gap', () => {
    const lanes = buildLanes(
      [
        ev(h(1), 'boot'),
        ev(h(2), 'sleep'),
        ev(h(3), 'shutdown'),
        ev(h(8), 'boot'),
        ev(h(9), 'resume'),
        ev(h(12), 'shutdown'),
      ],
      T0,
      T1,
      NOW,
    );

    const sleep = lane(lanes, 'sleep');
    expect(coversAt(sleep, H(2.5))).toBe(true); // slept before the power loss
    expect(coversAt(sleep, H(5))).toBe(false); // …but not while the host was off
    expect(coversAt(sleep, H(8.5))).toBe(true); // still "asleep" after boot, until resume
    expect(coversAt(sleep, H(10))).toBe(false); // resumed
    expect(containedIn(sleep, lane(lanes, 'power'))).toBe(true);
  });

  test('spans never escape the window', () => {
    const lanes = buildLanes(
      [ev(h(-5), 'boot'), ev(h(2), 'logon'), ev(h(3), 'active'), ev(h(30), 'idle')],
      T0,
      T1,
      NOW,
    );
    for (const l of lanes) {
      for (const s of l.spans) {
        expect(s.from).toBeGreaterThanOrEqual(T0);
        expect(s.to).toBeLessThanOrEqual(T1);
        expect(s.to).toBeGreaterThan(s.from);
      }
    }
  });

  test('spans on a lane never overlap', () => {
    const lanes = buildLanes(
      [
        ev(h(1), 'boot'),
        ev(h(2), 'active'),
        ev(h(3), 'idle'),
        ev(h(4), 'active'),
        ev(h(5), 'idle'),
        ev(h(6), 'shutdown'),
      ],
      T0,
      T1,
      NOW,
    );
    for (const l of lanes) {
      const sorted = [...l.spans].sort((a, b) => a.from - b.from);
      for (let i = 1; i < sorted.length; i++) {
        expect(sorted[i].from, `${l.key} spans overlap`).toBeGreaterThanOrEqual(sorted[i - 1].to);
      }
    }
  });
});

// ---------------------------------------------------------------------------
// Truncated fetches. `limit` drops the OLD end of the window (the backend
// orders `at DESC`), so the selected period can reach back further than the
// data does.
// ---------------------------------------------------------------------------

describe('coverage floor (limit-truncated fetch)', () => {
  // Without a floor this is the correct reading: an end with no matching start
  // means the state was already open when the window began.
  test('carry-in reaches the window start when the data is complete', () => {
    const lanes = buildLanes([ev(h(20), 'idle')], T0, T1, NOW);
    expect(coversAt(lane(lanes, 'active'), H(1))).toBe(true);
    expect(covered(lane(lanes, 'active'))).toBe(20);
  });

  // With a floor the same carry-in is a fabrication: we have no events from
  // before the floor, so "already open" is an assumption, not a reading. This
  // is the 2-day-window-at-limit-50 case, where every lane rendered as a solid
  // two-day claim built from a handful of recent events.
  test('carry-in stops at the coverage floor instead of fabricating the gap', () => {
    const lanes = buildLanes([ev(h(20), 'idle')], T0, T1, NOW, null, H(19));
    const active = lane(lanes, 'active');
    expect(coversAt(active, H(1))).toBe(false);
    expect(coversAt(active, H(18))).toBe(false);
    expect(covered(active)).toBe(1); // only 19h → 20h, the part actually covered
    for (const s of active.spans) expect(s.from).toBeGreaterThanOrEqual(H(19));
  });

  test('no lane paints before the floor, whatever the feed mix', () => {
    const lanes = buildLanes(
      [ev(h(20), 'idle'), ev(h(21), 'logoff'), ev(h(22), 'shutdown'), ev(h(23), 'resume')],
      T0,
      T1,
      NOW,
      null,
      H(19),
    );
    for (const l of lanes) {
      for (const s of l.spans) {
        expect(s.from, `${l.key} paints before the coverage floor`).toBeGreaterThanOrEqual(H(19));
      }
    }
  });

  // The case above contains `shutdown`, which makes `hasPower` true and so
  // only exercises the clip-to-power path. The backfilled lanes reach the
  // floor by a different route — `buildSpans`' own clamp, via the envelope —
  // and that is the path where a wrong floor would be least visible, since
  // power and session would just appear to start later. Pin it directly.
  test('the presence envelope respects the floor on a winlog-less host', () => {
    const lanes = buildLanes(
      [ev(h(20), 'idle'), ev(h(22), 'active')],
      T0,
      T1,
      NOW,
      null,
      H(19),
    );

    // Backfilled from the envelope (no power kinds at all)…
    expect(lane(lanes, 'power').spans.length).toBeGreaterThan(0);
    expect(lane(lanes, 'session').spans.length).toBeGreaterThan(0);
    // …but not one pixel before the floor, where the envelope's carry-in
    // would otherwise have reached t0.
    for (const key of ['power', 'session', 'active']) {
      expect(coversAt(lane(lanes, key), H(10)), `${key} paints before the floor`).toBe(false);
      for (const s of lane(lanes, key).spans) {
        expect(s.from).toBeGreaterThanOrEqual(H(19));
      }
    }
    expect(coversAt(lane(lanes, 'power'), H(21))).toBe(true); // still covered inside the floor
  });

  // Markers are evidence of a specific event, so they are bounded by coverage
  // too — there is no such thing as a marker we didn't receive.
  test('markers before the floor are dropped', () => {
    const lanes = buildLanes([ev(h(5), 'logon'), ev(h(20), 'logoff')], T0, T1, NOW, null, H(19));
    const kinds = lane(lanes, 'session').markers.map((m) => m.kind);
    expect(kinds).toContain('logoff');
    expect(kinds).not.toContain('logon');
  });

  test('a floor outside the window is clamped, not trusted blindly', () => {
    // Before the window: no truncation in effect, behave as if unset.
    const early = buildLanes([ev(h(20), 'idle')], T0, T1, NOW, null, T0 - 5 * 3_600_000);
    expect(coversAt(lane(early, 'active'), H(1))).toBe(true);
    // After the window end: degenerate, nothing to draw rather than inverted spans.
    expect(buildLanes([ev(h(20), 'idle')], T0, T1, NOW, null, T1 + 1)).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// The time axis.
// ---------------------------------------------------------------------------

describe('axis ticks and gridlines', () => {
  const DAY = 86_400_000;

  test('ticks land on round local times, never on raw fractions of the span', () => {
    // The window that produced the original report: an axis reading
    // 7/19 10:38 / 7/19 21:51 because it just divided the span in fifths.
    for (const ts of axisTicks(T0, T0 + 2 * DAY).map((t) => t.ts)) {
      const d = new Date(ts);
      expect(d.getMinutes()).toBe(0);
      expect(d.getSeconds()).toBe(0);
    }
  });

  test('midnight ticks are labelled with the date, the rest with the time', () => {
    const ticks = axisTicks(T0, T0 + 2 * DAY);
    const days = ticks.filter((t) => t.isDay);
    expect(days.length).toBeGreaterThan(0);
    for (const t of days) expect(t.label).toMatch(/^\d+\/\d+$/);
    for (const t of ticks.filter((x) => !x.isDay)) expect(t.label).toMatch(/^\d{2}:\d{2}$/);
  });

  test('every label sits on a gridline', () => {
    const grid = new Set(axisGridlines(T0, T0 + 2 * DAY).map((g) => g.ts));
    for (const t of axisTicks(T0, T0 + 2 * DAY)) expect(grid.has(t.ts)).toBe(true);
  });

  test('gridlines are finer than the labels, but not by more than one step', () => {
    const ticks = axisTicks(T0, T0 + 2 * DAY).length;
    const grid = axisGridlines(T0, T0 + 2 * DAY).length;
    expect(grid).toBeGreaterThan(ticks);
    expect(grid).toBeLessThanOrEqual(ticks * 3);
  });

  // The step table tops out at 28 days. Past ~168 days it can no longer keep
  // the count under the cap, so the step has to be computed — otherwise the
  // largest entry is used regardless and the strip fills with hundreds of
  // labels. Reachable from the Analytics widget, whose `from` has no floor.
  test('a very long window stays bounded instead of falling back to the widest step', () => {
    for (const days of [200, 400, 1000, 4000]) {
      const span = days * DAY;
      const ticks = axisTicks(T0, T0 + span);
      const grid = axisGridlines(T0, T0 + span);
      expect(ticks.length, `${days}d: too many ticks`).toBeLessThanOrEqual(8);
      expect(grid.length, `${days}d: too many gridlines`).toBeLessThanOrEqual(14);
      expect(ticks.length, `${days}d: no ticks at all`).toBeGreaterThan(1);
    }
  });

  test('a degenerate window produces no ticks', () => {
    expect(axisTicks(T1, T0)).toHaveLength(0);
    expect(axisGridlines(T1, T0)).toHaveLength(0);
    expect(axisTicks(T0, T0)).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// The live edge: open state is only known up to now / the last heartbeat.
// ---------------------------------------------------------------------------

describe('live edge and heartbeat gating', () => {
  test('an open span stops at now, not at the window end', () => {
    const now = H(6);
    const lanes = buildLanes([ev(h(1), 'boot')], T0, T1, now);
    const spans = lane(lanes, 'power').spans;
    expect(spans).toHaveLength(1);
    expect(spans[0].to).toBe(now); // not T1
    expect(spans[0].openEnd).toBe(true);
  });

  test('with no heartbeat, nothing is hatched (the Events page path)', () => {
    const lanes = buildLanes([ev(h(1), 'boot')], T0, T1, H(6));
    expect(lane(lanes, 'power').spans.some((s) => s.uncertain)).toBe(false);
  });

  test('an offline agent gets a solid head and a hatched tail', () => {
    const now = H(12);
    // Last heartbeat at 4h; the grace window closes well before `now`.
    const lanes = buildLanes([ev(h(1), 'boot')], T0, T1, now, h(4));
    const spans = lane(lanes, 'power').spans;

    const solid = spans.filter((s) => !s.uncertain);
    const hatched = spans.filter((s) => s.uncertain);
    expect(solid).toHaveLength(1);
    expect(hatched).toHaveLength(1);
    expect(solid[0].from).toBe(H(1));
    expect(solid[0].to).toBe(hatched[0].from); // they abut, no gap
    expect(hatched[0].to).toBe(now);
  });

  // The gate runs after the lane branch, so it covers the backfilled lanes
  // too — but every case above only exercises it on a winlog-fed `power`
  // lane. This is the shape that actually matters in the field: a host whose
  // power/session lanes are painted from the presence envelope (no winlog)
  // and whose agent has gone quiet.
  test('backfilled lanes are gated too, not just winlog-fed ones', () => {
    const now = H(12);
    const lanes = buildLanes([ev(h(2), 'active')], T0, T1, now, h(4));

    for (const key of ['power', 'session', 'active']) {
      const spans = lane(lanes, key).spans;
      expect(spans.some((s) => s.uncertain), `${key} should have a hatched tail`).toBe(true);
      expect(spans.some((s) => !s.uncertain), `${key} should keep a solid head`).toBe(true);
      // The hatched tail runs to the live edge and nothing is painted past it.
      const last = [...spans].sort((a, b) => a.to - b.to).at(-1)!;
      expect(last.to).toBe(now);
      expect(last.uncertain).toBe(true);
    }
  });

  // #970's whole point is that `idle` still proves the host was up. That must
  // survive the gate: the envelope is hatched past the heartbeat, not cut back
  // to the last `active`.
  test('the presence envelope is hatched past the heartbeat, not truncated', () => {
    const now = H(12);
    const lanes = buildLanes([ev(h(2), 'active'), ev(h(5), 'idle')], T0, T1, now, h(4));
    const power = lane(lanes, 'power');

    // Covered continuously to the live edge, part solid, part hatched.
    expect(coversAt(power, H(3))).toBe(true);
    expect(coversAt(power, H(8))).toBe(true);
    expect(coversAt(power, H(11.9))).toBe(true);
    expect(power.spans.some((s) => s.uncertain)).toBe(true);
    // …while the active lane still shows the real idle gap.
    expect(coversAt(lane(lanes, 'active'), H(8))).toBe(false);
  });

  // Heartbeats cadence at ~30s and are lossy by design (core NATS, never
  // queued), so the 2min grace absorbs a few dropped ticks. Without it a
  // healthy agent's strip would fray into hatching every time one went
  // missing.
  test('a heartbeat inside the grace window does not hatch anything', () => {
    const now = H(12);
    const oneMissedBeat = now - 60_000;
    expect(oneMissedBeat).toBeGreaterThan(now - AGENT_ACTIVE_THRESHOLD_MS);

    const lanes = buildLanes(
      [ev(h(1), 'boot'), ev(h(2), 'active')],
      T0,
      T1,
      now,
      new Date(oneMissedBeat).toISOString(),
    );
    for (const l of lanes) {
      expect(l.spans.some((s) => s.uncertain), `${l.key} should not be hatched`).toBe(false);
    }
  });

  test('just past the grace window, hatching starts', () => {
    const now = H(12);
    const justStale = now - AGENT_ACTIVE_THRESHOLD_MS - 60_000;
    const lanes = buildLanes(
      [ev(h(1), 'boot')],
      T0,
      T1,
      now,
      new Date(justStale).toISOString(),
    );
    expect(lane(lanes, 'power').spans.some((s) => s.uncertain)).toBe(true);
  });

  test('a closed historical span is never hatched, however stale the agent', () => {
    // boot/shutdown is a confirmed pair — the current heartbeat says nothing
    // about whether it happened.
    const lanes = buildLanes([ev(h(1), 'boot'), ev(h(3), 'shutdown')], T0, T1, H(12), h(2));
    const spans = lane(lanes, 'power').spans;
    expect(spans).toHaveLength(1);
    expect(spans[0].uncertain).toBeFalsy();
    expect(spans[0].to).toBe(H(3));
  });
});
