import { describe, expect, test } from 'bun:test';
import { AGENT_ACTIVE_THRESHOLD_MS } from '@/lib/utils';

import {
  agentDownRanges,
  axisGridlines,
  axisTicks,
  buildLanes,
  evidenceEdges,
  noEvidenceRanges,
  subtractRanges,
  swimlaneWindow,
} from './OperationalTimeline';

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
/** Winlog-sourced. `source` is what tells the strip a host runs the winlog
 *  collector, rather than inferring it from which kinds happen to be in the
 *  fetched window (#1256). */
const evw = (at: string, kind: string) => ({ at, kind, source: 'winlog:System' });
/** Agent idle-sampler sourced. */
const eva = (at: string, kind: string) => ({ at, kind, source: 'agent:internal' });

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

  // #1256. The matrix above varies WHICH FEED a host reports — a property of
  // the host. The code's branch key is which KINDS landed in this window — a
  // property of the fetch. Those are different axes, which is how a matrix
  // that reads exhaustive left the one cell that matters unexplored: partial
  // winlog (session/sleep, no reboot in-window) alongside a running sampler.
  //
  // `ev` carries no `source`, so these use `evw` / `eva` to say which
  // collector produced each event — the distinction the fix rests on.
  describe('partial winlog + sampler (the cell #1256 fell through)', () => {
    // A laptop that suspended overnight and never rebooted inside the window.
    const night = [
      evw(h(2), 'logon'), eva(h(3), 'active'), eva(h(8), 'idle'),
      evw(h(9), 'logoff'), evw(h(9), 'sleep'),
      evw(h(21), 'resume'), evw(h(21), 'logon'), eva(h(22), 'active'),
    ];

    test('session does not claim signed-in across the logged-off night', () => {
      const lanes = buildLanes(night, T0, T1, NOW);
      expect(coversAt(lane(lanes, 'session'), H(15))).toBe(false);
    });

    test('the sampler envelope does not paint where the sampler was silent', () => {
      const lanes = buildLanes(night, T0, T1, NOW);
      // No active/idle event exists between 8h and 22h. Anything painted at
      // 15h on a sampler-derived lane came from carry-in, i.e. from absence.
      expect(coversAt(lane(lanes, 'active'), H(15))).toBe(false);
    });

    // The trap this PR's first draft fell into: every assertion above probes
    // H(15), inside the stretch that is uncovered either way, so none of them
    // could tell "correctly reads as logged off" from "clipped everything to
    // nothing". Deriving `hasWinlog` from `source` decoupled it from whether
    // any power span exists, and `clipToOn(spans, [])` returns nothing — so
    // the genuine, winlog-confirmed session at h2–h9 vanished. Probe a
    // COVERED instant, not only an uncovered one.
    test('genuine winlog spans survive when there is no power span to clip to', () => {
      const lanes = buildLanes(night, T0, T1, NOW);
      expect(coversAt(lane(lanes, 'session'), H(5))).toBe(true);
      expect(coversAt(lane(lanes, 'active'), H(5))).toBe(true);
    });

    test('power is not synthesised from the envelope when winlog exists', () => {
      // Unseeded: winlog is demonstrably running (logon/sleep/resume carry
      // `winlog:*`) but says nothing about power inside the window. The old
      // predicate read that as "no winlog collector" and painted power from
      // the sampler envelope, carried in to both edges — across a night the
      // sampler never sampled. Nothing is the honest answer; the no-evidence
      // bands hatch it.
      const lanes = buildLanes(night, T0, T1, NOW);
      expect(coversAt(lane(lanes, 'power'), H(15))).toBe(false);
    });

    test('a seeded boot lets the power lane say ON without the envelope', () => {
      // What `/api/obs_events/lane_seeds` supplies: the newest power event
      // before the window. Sleep is not a power-off, so ON is correct here —
      // and it now comes from winlog rather than from the sampler.
      const lanes = buildLanes([evw(h(-5), 'boot'), ...night], T0, T1, NOW);
      expect(coversAt(lane(lanes, 'power'), H(15))).toBe(true);
      expect(coversAt(lane(lanes, 'sleep'), H(15))).toBe(true);
      expect(coversAt(lane(lanes, 'session'), H(15))).toBe(false);
    });

    test('a winlog-less host still gets the #970 envelope backfill', () => {
      // The behaviour the predicate exists to protect: sampler-only hosts
      // must keep power/session painted from the envelope.
      const lanes = buildLanes([eva(h(3), 'active'), eva(h(8), 'idle')], T0, T1, NOW);
      expect(coversAt(lane(lanes, 'power'), H(5))).toBe(true);
      expect(coversAt(lane(lanes, 'session'), H(5))).toBe(true);
    });
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

describe('recorded outages cut the lanes (#1245)', () => {
  // The reported shape: the sampler opens an `active` span in the evening,
  // the host suspends, and the closing `idle` never fires — the sampler
  // debounces over five minutes and the box is gone in ten seconds. The span
  // then ran to the NEXT MORNING's first idle and painted the night as work.
  const overnight = [
    ev(h(2), 'boot'),
    ev(h(3), 'active'),
    ev(h(9), 'agent_offline'), // the host went away here
    ev(h(9.02), 'sleep'), // backfilled next morning, stamped now
    ev(h(21), 'agent_online'), // and came back here
    ev(h(21.5), 'idle'), // the span the old code closed against
  ];

  test('the active lane does not paint across the outage', () => {
    const lanes = buildLanes(overnight, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'active'), H(15))).toBe(false);
  });

  test('what it does paint stops at the moment the agent went quiet', () => {
    const lanes = buildLanes(overnight, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'active'), H(8))).toBe(true);
    const evening = lane(lanes, 'active').spans.find((sp) => sp.from <= H(8) && H(8) < sp.to)!;
    expect(evening.to).toBe(H(9));
  });

  // The winlog lanes are late, not absent — so they must NOT be cut, and the
  // strip has to repaint the period once the backfill lands.
  test('a winlog lane is unknown while its backfill is still in flight', () => {
    const notYet = overnight.filter((e) => e.kind !== 'sleep');
    const lanes = buildLanes(notYet, T0, T1, NOW);
    const bands = noEvidenceRanges(T0, T1, NOW, T0, undefined, undefined, notYet);
    // Nothing covers the night on the sleep lane, so the band shows through.
    expect(coversAt(lane(lanes, 'sleep'), H(15))).toBe(false);
    const visible = subtractRanges(bands, lane(lanes, 'sleep').spans);
    expect(visible.some((b) => b.from <= H(15) && H(15) < b.to)).toBe(true);
  });

  test('and repaints it once the backfilled record arrives', () => {
    // Same host, same window, one late `sleep` record later. No special
    // case makes this work: the lanes are rebuilt from whatever events the
    // fetch returned, so a record that arrives afterwards simply paints.
    const arrived = [...overnight, ev(h(21), 'resume')];
    const lanes = buildLanes(arrived, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'sleep'), H(15))).toBe(true);
    const bands = noEvidenceRanges(T0, T1, NOW, T0, undefined, undefined, arrived);
    const visible = subtractRanges(bands, lane(lanes, 'sleep').spans);
    expect(visible.some((b) => b.from <= H(15) && H(15) < b.to)).toBe(false);
  });

  // The other half of the rule, and the reason it is a cut rather than a
  // subtraction: every producer shares one outbox, so a NATS outage does not
  // stop the agent observing — it stops the backend hearing. Samples taken
  // during the outage arrive afterwards with their original timestamps and
  // are real. Subtracting the outage would erase them wholesale.
  test('samples produced during a link-only outage still paint', () => {
    const linkDown = [
      ev(h(2), 'boot'),
      ev(h(9), 'agent_offline'),
      ev(h(11), 'active'), // the agent kept sampling; delivery was late
      ev(h(13), 'idle'),
      ev(h(15), 'active'),
      ev(h(17), 'idle'),
      ev(h(21), 'agent_online'),
    ];
    const lanes = buildLanes(linkDown, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'active'), H(12))).toBe(true);
    expect(coversAt(lane(lanes, 'active'), H(16))).toBe(true);
    expect(coversAt(lane(lanes, 'active'), H(14))).toBe(false); // the idle gap
  });

  // The reported ORDER, which `overnight` above does not have: the closing
  // `idle` landed at 09:16 and `agent_online` at 09:18, so the event that
  // closes the span is stamped INSIDE the outage, two minutes before the
  // agent was recorded back. Any rule that only cuts when the close falls
  // after the outage's own end leaves this untouched — the whole bug, intact,
  // with the suite still green. Pin the real ordering so that stays visible.
  test('the closing event is inside the outage in the reported case', () => {
    const asReported = [
      ev(h(18.85), 'idle'),
      ev(h(18.87), 'active'), // the span that painted the night
      ev(h(19.14), 'logoff'),
      ev(h(19.14), 'agent_offline'),
      ev(h(19.145), 'sleep'),
      ev(h(19.148), 'resume'),
      ev(h(21.5), 'idle'), // 09:16 — closes the span, still inside the outage
      ev(h(21.6), 'agent_online'), // 09:18
    ];
    const lanes = buildLanes(asReported, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'active'), H(20))).toBe(false);
    const evening = lane(lanes, 'active').spans.find((sp) => sp.from <= H(19) && H(19) < sp.to)!;
    expect(evening.to).toBe(H(19.14));
  });

  // A span that OPENS during an outage is a different case, and the one the
  // "still paint" test above covers: its opening event is itself stamped
  // inside the outage, which is positive proof the agent was sampling then.
  // A span that opens BEFORE and closes during has no such proof — the only
  // thing carrying it across the silence is the absence of a transition, and
  // absence is what a dead agent produces. It is cut, deliberately: the
  // operator gets "unknown" for a stretch nobody observed rather than a
  // confident claim that may be hours of invented work.
  test('a span opened before the outage is cut even if its close lands inside', () => {
    const closesInside = [
      ev(h(2), 'boot'),
      ev(h(10), 'active'), // opens five minutes before the agent goes quiet
      ev(h(10.083), 'agent_offline'),
      ev(h(12.5), 'idle'), // and is closed from within the silent stretch
      ev(h(13), 'agent_online'),
    ];
    const lanes = buildLanes(closesInside, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'active'), H(11))).toBe(false);
    expect(coversAt(lane(lanes, 'active'), H(10.04))).toBe(true);
  });

  // The over-correction the first fix shipped, caught in the demo rather than
  // here: cutting EVERY span that bridged the outage also cut the winlog ones.
  // A `sleep`→`resume` pair records both ends, so it asserts the night on its
  // own terms and stays whole; only the sampler's silence-based spans go.
  // Note the sleep opens BEFORE the agent went quiet — the same shape that
  // makes the active span wrong is what makes this one right.
  test('an OS-recorded pair that opened before the outage is not cut', () => {
    const slept = [
      ev(h(2), 'boot'),
      ev(h(3), 'active'),
      ev(h(8.9), 'sleep'), // the machine suspends…
      ev(h(9), 'agent_offline'), // …and the agent goes quiet with it
      ev(h(21), 'agent_online'),
      ev(h(21.1), 'resume'), // the OS logged the other end
      ev(h(21.5), 'idle'),
    ];
    const lanes = buildLanes(slept, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'sleep'), H(15))).toBe(true);
    expect(coversAt(lane(lanes, 'power'), H(15))).toBe(true); // sleep ⊆ power
    expect(coversAt(lane(lanes, 'active'), H(15))).toBe(false); // but this one still goes
  });

  test('the unknown band survives instead of being erased by the span', () => {
    // The draw site is `subtractRanges(noEvidence, lane.spans)`. Before the
    // lanes were cut, the false span covered the band completely and the
    // operator saw a solid, confident lie. Assert the band is still there
    // after that subtraction — the thing the screen actually renders.
    const lanes = buildLanes(overnight, T0, T1, NOW);
    const bands = noEvidenceRanges(T0, T1, NOW, T0, undefined, undefined, overnight);
    const visible = subtractRanges(bands, lane(lanes, 'active').spans);
    expect(visible.some((b) => b.from <= H(15) && H(15) < b.to)).toBe(true);
  });
});

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
  // The invariant that was missing, and the reason #1326 could be drawn for
  // as long as it was. The suite related each subordinate lane to `power`
  // (`sleep ⊆ power`, `active ⊆ power`) and never related the two to each
  // other — so "asleep and interactive at the same instant" broke no rule.
  test('active and sleep never overlap, in every feed combination', () => {
    const cases: { name: string; events: { at: string; kind: string }[]; cover?: number }[] = [
      {
        name: 'full winlog',
        events: [
          ev(h(1), 'boot'),
          ev(h(2), 'active'),
          ev(h(5), 'idle'),
          ev(h(6), 'sleep'),
          ev(h(7), 'resume'),
          ev(h(8), 'active'),
          ev(h(10), 'idle'),
        ],
      },
      {
        name: 'the sampler never closed its span before the suspend (#1245 shape)',
        events: [ev(h(1), 'boot'), ev(h(2), 'active'), ev(h(6), 'sleep'), ev(h(20), 'resume')],
      },
      {
        name: 'a truncated fetch, first sleep-lane record is a resume (#1326)',
        events: [
          ev(h(2.2), 'boot'),
          ev(h(2.5), 'active'),
          ev(h(4), 'idle'),
          ev(h(5), 'active'),
          ev(h(6), 'logoff'),
          ev(h(20), 'resume'),
        ],
        cover: H(2),
      },
    ];
    for (const c of cases) {
      const lanes = buildLanes(c.events, T0, T1, NOW, undefined, c.cover);
      const sleep = lane(lanes, 'sleep').spans;
      const active = lane(lanes, 'active').spans;
      const overlap = sleep.reduce(
        (sum, s) =>
          sum + active.reduce((n, a) => n + Math.max(0, Math.min(s.to, a.to) - Math.max(s.from, a.from)), 0),
        0,
      );
      expect(overlap / 60_000, `${c.name}: minutes asleep AND active`).toBe(0);
    }
  });

  // The #1326 case on its own, stated in the terms the bug was reported in.
  // A laptop suspended the night before; its morning `resume` fell in the
  // stretch a `limit` fetch dropped; so the first sleep-lane record left in
  // the set is the NEXT day's resume, and `buildSpans` carries in against it.
  // The strip drew four hours of "asleep" over a working afternoon, with a
  // `logoff` marker sitting inside the span.
  test('a carried-in sleep stops at the first event the host produced', () => {
    const truncated = [
      ev(h(2.2), 'boot'),
      ev(h(2.5), 'active'), // the machine is demonstrably awake here…
      ev(h(4), 'idle'),
      ev(h(5), 'active'),
      ev(h(6), 'logoff'),
      ev(h(20), 'resume'), // …yet this is the first sleep-lane record we hold
    ];
    const lanes = buildLanes(truncated, T0, T1, NOW, undefined, H(2));
    expect(coversAt(lane(lanes, 'sleep'), H(10))).toBe(false);
    expect(coversAt(lane(lanes, 'sleep'), H(5))).toBe(false);
    // What survives is only the stretch nothing contradicts.
    const s = lane(lanes, 'sleep').spans;
    expect(s).toHaveLength(1);
    expect(s[0].to).toBe(H(2.5));
  });

  // `subtractRanges` copies the source span's fields onto both pieces, so a
  // split has to repair the edge flags. Without it the left piece keeps
  // `openEnd` and the right keeps `openStart`, and the tooltip tells the
  // operator the state runs off the window edge when it stops at a `sleep`.
  test('a split active span does not claim open edges at the cut', () => {
    const split = [
      ev(h(1), 'boot'),
      ev(h(2), 'active'), // never closed by an `idle` — genuinely open-ended
      ev(h(5), 'sleep'),
      ev(h(6), 'resume'),
    ];
    // Heartbeat at the window end, so `gateToHeartbeat` splits nothing and
    // the only split under test is the suspend.
    const lanes = buildLanes(split, T0, T1, NOW, new Date(T1).toISOString());
    const spans = lane(lanes, 'active').spans;
    const left = spans.find((s) => s.from === H(2))!;
    const right = spans.find((s) => s.from === H(6))!;
    expect(left.to).toBe(H(5));
    expect(left.openEnd).toBe(false); // it stops at the suspend, not the edge
    expect(right.openStart).toBe(false); // and this one starts at the resume
    expect(right.openEnd).toBe(true); // the far edge is still genuinely open
  });

  // A repeated `sleep` is not proof the host woke. `buildSpans` already
  // decided that ("a second start while already open is ignored — a missed
  // end shouldn't fragment the interval"), and the contradiction cut was
  // undoing it silently: an eight-hour suspend with one duplicate record
  // half an hour in came out as a thirty-minute span. Modern standby can log
  // several sleep-kind transitions for one user-perceived suspend, and
  // delivery is at-least-once, so this is ordinary rather than exotic.
  test('a duplicate sleep record does not cut the suspend it repeats', () => {
    const duplicated = [
      ev(h(1), 'boot'),
      ev(h(2), 'sleep'),
      ev(h(2.5), 'sleep'), // again, with no `resume` between
      ev(h(10), 'resume'),
    ];
    const lanes = buildLanes(duplicated, T0, T1, NOW, new Date(T1).toISOString());
    expect(covered(lane(lanes, 'sleep'))).toBe(8);
    expect(coversAt(lane(lanes, 'sleep'), H(6))).toBe(true);
  });

  // …but a genuine overnight suspend must survive, and the watchdog's own
  // `agent_offline` lands inside every one of them. It is written by the
  // BACKEND because the machine went quiet, so it is not the host producing
  // an event and must not count as a contradiction.
  test('the watchdog record does not cut a real overnight sleep', () => {
    const overnight = [
      ev(h(1), 'boot'),
      ev(h(2), 'active'),
      ev(h(5), 'idle'),
      ev(h(6), 'logoff'),
      ev(h(6.1), 'sleep'),
      ev(h(6.2), 'agent_offline'), // inside the sleep, by construction
      ev(h(20), 'agent_online'),
      ev(h(20.1), 'resume'),
    ];
    const lanes = buildLanes(overnight, T0, T1, NOW);
    expect(coversAt(lane(lanes, 'sleep'), H(15))).toBe(true);
  });

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
// The no-evidence band is subtracted per lane so a stretch never claims two
// things at once.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The shared window. Lived inline in an Events.tsx `useMemo` until the
// `all`-preset case below shipped broken.
// ---------------------------------------------------------------------------

describe('swimlaneWindow', () => {
  const op = (n: number, pc = 'pc1') => ({ at: h(n), pc_id: pc });

  test('a period with a lower bound wins outright, data or no data', () => {
    const w = ' 2026-07-19T00:00:00.000Z';
    expect(swimlaneWindow(w, undefined, ['pc1'], [op(3)], [op(3)])).toEqual([w, undefined]);
    expect(swimlaneWindow(w, 'X', [], [], [])).toEqual([w, 'X']);
  });

  test('without a period, the kept PCs’ operational events set the extent', () => {
    const [f, t] = swimlaneWindow(undefined, undefined, ['pc1'], [op(4), op(8)], []);
    // Padded outwards, so the bounds sit outside the events.
    expect(Date.parse(f!)).toBeLessThan(H(4));
    expect(Date.parse(t!)).toBeGreaterThan(H(8));
  });

  test('events for PCs that are not rendered do not widen the window', () => {
    const [f] = swimlaneWindow(undefined, undefined, ['pc1'], [op(4, 'pc2'), op(8), op(9)], []);
    expect(Date.parse(f!)).toBeGreaterThan(H(4));
  });

  // The bug this extraction exists for. A pinned host with no operational
  // events yielded no bounds, so the strip had no axis, tripped its own
  // `t1 <= t0` guard and rendered the plain "no events" note — the pin
  // achieving nothing on exactly the preset ("all") used to ask whether a
  // host ever reported.
  test('with no operational events it falls back to the whole response', () => {
    const other = [{ at: h(2) }, { at: h(10) }];
    const [f, t] = swimlaneWindow(undefined, undefined, ['pc1'], [], other);
    expect(f).toBeDefined();
    expect(t).toBeDefined();
    expect(Date.parse(f!)).toBeLessThan(H(2));
    expect(Date.parse(t!)).toBeGreaterThan(H(10));
  });

  test('no events of any kind yields no window rather than an invented one', () => {
    expect(swimlaneWindow(undefined, undefined, ['pc1'], [], [])).toEqual([undefined, undefined]);
  });

  test('a single instant yields no window (nothing to span)', () => {
    expect(swimlaneWindow(undefined, undefined, ['pc1'], [op(4)], [])).toEqual([
      undefined,
      undefined,
    ]);
  });

  test('unparseable timestamps are ignored, not propagated as NaN bounds', () => {
    const [f, t] = swimlaneWindow(
      undefined,
      undefined,
      ['pc1'],
      [{ at: 'not-a-date', pc_id: 'pc1' }, op(4), op(8)],
      [],
    );
    expect(Number.isNaN(Date.parse(f!))).toBe(false);
    expect(Number.isNaN(Date.parse(t!))).toBe(false);
  });
});

describe('noEvidenceRanges', () => {
  const DAY = 86_400_000;

  test('a healthy agent with a complete fetch has no unknown stretches', () => {
    expect(noEvidenceRanges(T0, T1, H(12), T0, new Date(H(12) - 30_000).toISOString(), H(11)))
      .toEqual([]);
  });

  test('truncation alone yields one band at the window start', () => {
    const out = noEvidenceRanges(T0, T1, H(12), H(6), new Date(H(12) - 30_000).toISOString(), H(11));
    expect(out).toEqual([{ from: T0, to: H(6), reason: 'truncated' }]);
  });

  test('a stale agent alone yields one band at the live edge', () => {
    const out = noEvidenceRanges(T0, T1, H(12), T0, new Date(H(4)).toISOString(), H(4));
    expect(out).toHaveLength(1);
    expect(out[0].reason).toBe('offline');
    expect(out[0].to).toBe(H(12));
  });

  // The overlap CodeRabbit found. The coverage floor is global across PCs, so
  // a host quiet since yesterday sits behind a floor set by a busier host's
  // recent events — `certainEdge` then lands before `coverEdge` and the two
  // bands would cover the same pixels with different reasons.
  test('a long-offline agent behind a recent coverage floor produces no overlap', () => {
    const out = noEvidenceRanges(T0, T1, H(20), H(12), new Date(T0 - DAY).toISOString(), H(13));
    for (let i = 1; i < out.length; i++) {
      expect(out[i].from).toBeGreaterThanOrEqual(out[i - 1].to);
    }
    expect(out.find((r) => r.reason === 'offline')?.from).toBe(H(12));
  });

  // No operational events at all — reachable only for a strip that exists
  // without events, i.e. a PC the operator named explicitly. Every lane is
  // blank, and a blank lane otherwise reads as "measured, and it was off".
  // Nothing was measured, so the whole window is unknown and `noEvents` is
  // the reason that actually explains it (it subsumes truncated/offline,
  // which are true but not the point).
  test('no events at all: the whole window is unknown, whatever the heartbeat', () => {
    for (const hb of [undefined, null, new Date(H(4)).toISOString()]) {
      const out = noEvidenceRanges(T0, T1, H(20), H(12), hb, undefined);
      expect(out).toEqual([{ from: T0, to: H(20), reason: 'noEvents' }]);
    }
  });

  test('no events and a degenerate window yields nothing rather than an inverted range', () => {
    expect(noEvidenceRanges(T0, T1, T0, T0, null, undefined)).toEqual([]);
  });

  // A live agent does not rescue it: a heartbeat proves the host is up *now*,
  // not what any lane was doing across a window with no transitions in it.
  test('a fresh heartbeat does not make an event-less window certain', () => {
    const out = noEvidenceRanges(T0, T1, H(20), T0, new Date(H(20) - 30_000).toISOString(), undefined);
    expect(out).toHaveLength(1);
    expect(out[0].reason).toBe('noEvents');
  });

  test('bands never overlap across a sweep of floor / heartbeat combinations', () => {
    for (const cover of [T0, H(2), H(12), H(23)]) {
      for (const hb of [undefined, null, new Date(T0 - DAY).toISOString(), new Date(H(6)).toISOString()]) {
        for (const last of [undefined, H(1), H(13)]) {
          const out = noEvidenceRanges(T0, T1, H(20), cover, hb, last);
          for (const r of out) expect(r.to).toBeGreaterThan(r.from);
          for (let i = 1; i < out.length; i++) {
            expect(out[i].from, `overlap for cover=${cover} hb=${hb} last=${last}`)
              .toBeGreaterThanOrEqual(out[i - 1].to);
          }
        }
      }
    }
  });
});

// ---------------------------------------------------------------------------
// Recorded outages (#1089). The live-edge gate can only describe "now";
// these describe a stretch that has since become history.
// ---------------------------------------------------------------------------

describe('agentDownRanges', () => {
  const LIVE = H(24);

  test('an outage closes at the recovery event', () => {
    const out = agentDownRanges(
      [ev(h(4), 'agent_offline'), ev(h(9), 'agent_online')],
      T0,
      T1,
      LIVE,
    );
    expect(out).toEqual([{ from: H(4), to: H(9), reason: 'agentDown' }]);
  });

  // This test used to assert the opposite — "any later event closes it" —
  // on the reasoning that "anything arriving from that host proves the agent
  // was running again". That is true of DELIVERY and false of TIMESTAMPS,
  // and the strip is drawn on timestamps. Winlog is read out of the Windows
  // event log after the agent returns, up to 24 h of it, so a record stamped
  // seconds after the agent died arrives hours later. The implementation
  // comment stated the same premise, so test and code agreed with each other
  // and the pair could never fail — a fourteen-hour blackout rendered as ten
  // seconds in production while this suite stayed green (#1245).
  test('a backfilled winlog event does not close an outage', () => {
    const out = agentDownRanges(
      [
        ev(h(4), 'agent_offline'),
        // Stamped a minute later, delivered with the 09:00 backfill.
        ev(h(4.02), 'sleep'),
        ev(h(4.03), 'resume'),
        ev(h(9), 'agent_online'),
      ],
      T0,
      T1,
      LIVE,
    );
    expect(out).toEqual([{ from: H(4), to: H(9), reason: 'agentDown' }]);
  });

  // Only the watchdog's own kinds close it, because only those are emitted
  // from live heartbeats rather than reconstructed after the fact.
  test('a boot does not close an outage either', () => {
    const out = agentDownRanges([ev(h(4), 'agent_offline'), ev(h(7), 'boot')], T0, T1, LIVE);
    expect(out).toEqual([{ from: H(4), to: LIVE, reason: 'agentDown' }]);
  });

  test('an outage with nothing after it runs to the live edge', () => {
    const out = agentDownRanges([ev(h(4), 'agent_offline')], T0, T1, H(20));
    expect(out).toEqual([{ from: H(4), to: H(20), reason: 'agentDown' }]);
  });

  // Two outages must not merge into one long stretch across the interval
  // where the host was demonstrably back.
  test('separate outages stay separate', () => {
    const out = agentDownRanges(
      [
        ev(h(2), 'agent_offline'),
        ev(h(4), 'agent_online'),
        ev(h(8), 'agent_offline'),
        ev(h(10), 'agent_online'),
      ],
      T0,
      T1,
      LIVE,
    );
    expect(out).toEqual([
      { from: H(2), to: H(4), reason: 'agentDown' },
      { from: H(8), to: H(10), reason: 'agentDown' },
    ]);
  });

  // A missed recovery marker (backend restarted mid-outage, say) must not
  // swallow the second outage's start.
  test('a second offline with no recovery between closes the first', () => {
    const out = agentDownRanges(
      [ev(h(2), 'agent_offline'), ev(h(8), 'agent_offline')],
      T0,
      T1,
      H(20),
    );
    expect(out).toEqual([{ from: H(2), to: H(20), reason: 'agentDown' }]);
  });

  // The backend emits a recovery and a re-drop at the same instant when an
  // agent dies again inside one sweep interval. The pair must not make the
  // scan stall on its own timestamp — the second outage still has to close at
  // the next watchdog event.
  test('a recovery and re-drop sharing an instant do not stall the scan', () => {
    const out = agentDownRanges(
      [
        ev(h(2), 'agent_offline'),
        ev(h(6), 'agent_online'),
        ev(h(6), 'agent_offline'),
        ev(h(9), 'agent_online'),
      ],
      T0,
      T1,
      LIVE,
    );
    // [2,6] and [6,9] touch, so they coalesce — there is no measurable
    // uptime to draw between them. What matters is that the range ends at
    // the recovery, not that it runs to the live edge.
    expect(out).toEqual([{ from: H(2), to: H(9), reason: 'agentDown' }]);
  });

  test('input order does not matter', () => {
    const shuffled = agentDownRanges(
      [ev(h(9), 'agent_online'), ev(h(4), 'agent_offline')],
      T0,
      T1,
      LIVE,
    );
    expect(shuffled).toEqual([{ from: H(4), to: H(9), reason: 'agentDown' }]);
  });

  test('an outage is clipped to the window', () => {
    const out = agentDownRanges([ev(h(-5), 'agent_offline')], T0, T1, H(30));
    expect(out[0].from).toBe(T0);
    expect(out[0].to).toBe(T1);
  });

  test('no offline events yields nothing', () => {
    expect(agentDownRanges([ev(h(4), 'boot'), ev(h(5), 'logon')], T0, T1, LIVE)).toEqual([]);
  });
});

describe('recorded outages inside noEvidenceRanges', () => {
  const laneEvents = [ev(h(1), 'boot'), ev(h(20), 'shutdown')];

  test('a recorded outage becomes an unknown stretch', () => {
    const out = noEvidenceRanges(T0, T1, H(23), T0, new Date(H(23)).toISOString(), H(20), [
      ...laneEvents,
      ev(h(5), 'agent_offline'),
      ev(h(8), 'agent_online'),
    ]);
    const down = out.filter((r) => r.reason === 'agentDown');
    expect(down).toEqual([{ from: H(5), to: H(8), reason: 'agentDown' }]);
  });

  // An agent that dropped and never returned has BOTH a recorded outage and
  // a stale live edge. Overlapping entries would stack two elements and leave
  // the displayed reason to paint order.
  test('a recorded outage overlapping the live-edge stretch stays disjoint', () => {
    const out = noEvidenceRanges(T0, T1, H(23), T0, new Date(H(6)).toISOString(), H(6), [
      ...laneEvents,
      ev(h(6), 'agent_offline'),
    ]);
    for (let i = 1; i < out.length; i++) {
      expect(out[i].from).toBeGreaterThanOrEqual(out[i - 1].to);
    }
    for (const r of out) expect(r.to).toBeGreaterThan(r.from);
  });

  // Observation events say when we were listening, never what a lane was
  // doing — so a host with only those still has no lane evidence.
  test('observation events alone do not count as lane evidence', () => {
    const out = noEvidenceRanges(T0, T1, H(23), T0, new Date(H(23)).toISOString(), H(8), [
      ev(h(5), 'agent_offline'),
      ev(h(8), 'agent_online'),
    ]);
    expect(out).toEqual([{ from: T0, to: H(23), reason: 'noEvents' }]);
  });
});

describe('subtractRanges', () => {
  const r = (from: number, to: number) => ({ from, to });

  test('a cut through the middle leaves both ends', () => {
    expect(subtractRanges([r(0, 10)], [r(4, 6)])).toEqual([r(0, 4), r(6, 10)]);
  });

  test('a cut covering everything leaves nothing', () => {
    expect(subtractRanges([r(0, 10)], [r(0, 10)])).toEqual([]);
    expect(subtractRanges([r(2, 8)], [r(0, 10)])).toEqual([]);
  });

  test('disjoint and touching cuts leave the range whole', () => {
    expect(subtractRanges([r(0, 10)], [r(10, 20)])).toEqual([r(0, 10)]);
    expect(subtractRanges([r(0, 10)], [r(-5, 0)])).toEqual([r(0, 10)]);
  });

  test('several cuts apply cumulatively', () => {
    expect(subtractRanges([r(0, 10)], [r(2, 3), r(6, 7)])).toEqual([
      r(0, 2),
      r(3, 6),
      r(7, 10),
    ]);
  });

  test('an empty cut is a no-op, and zero-width input is dropped', () => {
    expect(subtractRanges([r(0, 10)], [])).toEqual([r(0, 10)]);
    expect(subtractRanges([r(5, 5)], [])).toEqual([]);
    expect(subtractRanges([r(0, 10)], [r(4, 4)])).toEqual([r(0, 10)]);
  });

  test('split pieces keep the source range’s other fields', () => {
    const out = subtractRanges([{ from: 0, to: 10, reason: 'offline' }], [r(4, 6)]);
    expect(out.map((x) => x.reason)).toEqual(['offline', 'offline']);
  });

  // The bug this exists to prevent: the unconfirmed hatch is a gradient with
  // transparent gaps, so a band drawn underneath shows through and the two
  // patterns interleave — one stretch asserting both "believed on" and "no
  // evidence". Subtracting the lane's own spans first means the band only
  // ever covers pixels the lane says nothing about.
  test('a lane with a span covering the whole band gets no band at all', () => {
    const band = [{ from: 100, to: 200, reason: 'offline' }];
    expect(subtractRanges(band, [r(100, 200)])).toEqual([]);
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

  // The renderer draws the no-evidence band from these edges while
  // `buildLanes` gates spans from them, so they have to agree — the band is
  // what makes an *empty* lane read as unknown instead of as a measured
  // "off", which spans alone can never express (#1086).
  describe('evidenceEdges', () => {
    test('no heartbeat: the edges coincide, so nothing is unknown', () => {
      const { liveEdge, certainEdge } = evidenceEdges(T1, H(12));
      expect(liveEdge).toBe(H(12));
      expect(certainEdge).toBe(liveEdge);
    });

    test('a fresh heartbeat leaves no unknown stretch', () => {
      const now = H(12);
      const { liveEdge, certainEdge } = evidenceEdges(T1, now, new Date(now - 30_000).toISOString());
      expect(certainEdge).toBe(liveEdge);
    });

    test('a stale heartbeat opens an unknown stretch ending at the live edge', () => {
      const now = H(12);
      const hb = H(4);
      const { liveEdge, certainEdge } = evidenceEdges(T1, now, new Date(hb).toISOString());
      expect(certainEdge).toBe(hb + AGENT_ACTIVE_THRESHOLD_MS);
      expect(liveEdge).toBe(now);
      expect(liveEdge).toBeGreaterThan(certainEdge);
    });

    test('the live edge never runs past now, even for a window ending later', () => {
      const now = H(6);
      expect(evidenceEdges(T1, now).liveEdge).toBe(now);
    });

    test('a heartbeat from the future cannot push certainty past the live edge', () => {
      const now = H(6);
      const { liveEdge, certainEdge } = evidenceEdges(T1, now, new Date(H(20)).toISOString());
      expect(certainEdge).toBe(liveEdge);
    });

    // `undefined` and `null` are different facts and must not collapse:
    // undefined = "not told yet" (query in flight), null = "the server says
    // this agent has never reported". `agents.last_heartbeat` is genuinely
    // nullable, so the second is reachable — and it's the case that most
    // needs the honesty, which is why treating it as fully trusted was wrong.
    test('undefined heartbeat leaves the strip ungated (nothing known yet)', () => {
      const { liveEdge, certainEdge } = evidenceEdges(T1, H(12), undefined, H(3));
      expect(certainEdge).toBe(liveEdge);
    });

    test('a null heartbeat trusts only as far as the newest event', () => {
      const { liveEdge, certainEdge } = evidenceEdges(T1, H(12), null, H(3));
      expect(certainEdge).toBe(H(3));
      expect(liveEdge).toBe(H(12));
    });

    test('a null heartbeat with no events at all trusts nothing', () => {
      const { certainEdge } = evidenceEdges(T1, H(12), null, undefined);
      expect(certainEdge).toBe(0);
    });

    test('a null heartbeat cannot trust past the live edge', () => {
      // A newest-event stamp in the future (clock skew) must not push
      // certainty past now.
      const { liveEdge, certainEdge } = evidenceEdges(T1, H(6), null, H(20));
      expect(certainEdge).toBe(liveEdge);
    });

    test('a never-reporting agent gets its open span hatched from the last event', () => {
      const lanes = buildLanes([ev(h(1), 'boot'), ev(h(3), 'logon')], T0, T1, H(12), null);
      const spans = lane(lanes, 'power').spans;
      const solid = spans.filter((s) => !s.uncertain);
      const hatched = spans.filter((s) => s.uncertain);
      expect(solid).toHaveLength(1);
      expect(hatched).toHaveLength(1);
      // Certainty ends at the newest event (the logon at 3h), not at `now`.
      expect(solid[0].to).toBe(H(3));
      expect(hatched[0].from).toBe(H(3));
    });

    test('the edges match what buildLanes gates on', () => {
      const now = H(12);
      const hb = new Date(H(4)).toISOString();
      const { certainEdge } = evidenceEdges(T1, now, hb);
      const lanes = buildLanes([ev(h(1), 'boot')], T0, T1, now, hb);
      const hatched = lane(lanes, 'power').spans.find((s) => s.uncertain)!;
      expect(hatched.from).toBe(certainEdge);
    });
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
