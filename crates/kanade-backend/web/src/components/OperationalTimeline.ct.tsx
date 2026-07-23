import { expect, test } from '@playwright/experimental-ct-react';

import { OperationalTimeline } from './OperationalTimeline';

// Issue #1094: three bugs shipped past a green `bun test` suite across
// #1084 / #1088 / #1093 because they were layout / hit-testing / perception,
// which jsdom (no layout engine, no `elementFromPoint`, no compositing)
// structurally cannot see. This file grows the browser coverage the issue
// proposes, one assertion class per describe:
//   - hit-testing: every `title` element can actually receive a hover
//   - non-overlap: no lane paints a no-evidence band over a span (the plaid)
// Still deferred to a later PR: screenshot baselines for the perceptual
// distinctions (sub-pixel bands, grey-vs-blue hatch at low alpha).
//
// The window is anchored to the current instant, NOT a fixed calendar date.
// `OperationalTimeline` reads `Date.now()` internally (the live / evidence
// edges) and can't be injected — so the window has to be *relative* to now to
// stay in the past on any clock. A fixed future-ish date fails exactly where
// it matters: a CI runner whose wall clock predates it sees the whole window
// in the future, `liveEdge = min(t1, now)` collapses to `now`, and the offline
// band never renders — green locally, red in CI. (That regression is why this
// comment is this long.)
//
// The offsets are chosen to draw every title-bearing element class:
//   - solid spans (power / session / sleep segments) and their markers
//   - an unconfirmed hatch tail: the open power span past a stale heartbeat
//   - a truncated no-evidence band: `coverageFrom` sits after the window
//     start, so `[from, coverageFrom)` is "fetch truncated, no data here"
//   - an offline no-evidence band: the stale heartbeat leaves the live edge
//     unvouched-for, so `[lastHeartbeat + grace, liveEdge]` is unknown
const HOUR = 3_600_000;
const NOW = Date.now();
const iso = (deltaMs: number) => new Date(NOW + deltaMs).toISOString();

const FROM = iso(-20 * HOUR);
const TO = iso(-60_000); // a minute ago, so it's firmly in the past on any clock
// After FROM, so `coverEdge > t0` and a truncated band renders at the head.
// Not driven by the first event's time — truncation is a property of how far
// back the fetch reached, which the component only learns from this prop.
const COVERAGE_FROM = iso(-19 * HOUR);
const LAST_HEARTBEAT = iso(-6 * HOUR); // stale vs TO → offline tail
const EVENTS = [
  { at: iso(-18 * HOUR), kind: 'boot' },
  { at: iso(-17.5 * HOUR), kind: 'logon' },
  { at: iso(-12 * HOUR), kind: 'lock' },
  { at: iso(-11 * HOUR), kind: 'unlock' },
  { at: iso(-8 * HOUR), kind: 'sleep' },
  { at: iso(-7 * HOUR), kind: 'resume' },
];

test.describe('OperationalTimeline hit-testing', () => {
  test('every element carrying a title is reachable at its own centre', async ({ mount, page }) => {
    await mount(
      <div style={{ width: 900 }}>
        <OperationalTimeline
          events={EVENTS}
          from={FROM}
          to={TO}
          coverageFrom={COVERAGE_FROM}
          lastHeartbeat={LAST_HEARTBEAT}
        />
      </div>,
    );

    // Sanity first: prove the fixture actually drew the rich state we mean to
    // guard, so a future change that quietly stops rendering bands can't turn
    // this into a vacuous green. The no-evidence band is the 135° hatch (the
    // uncertain-span hatch is 45°) — detecting it by that geometry keeps the
    // check independent of translated copy. Both band causes must appear: a
    // truncated band flush to the track's left edge (`[FROM, coverageFrom)`)
    // and an offline band that is NOT (`[lastHeartbeat + grace, liveEdge]`),
    // so the fixture genuinely exercises the truncated path, not just offline.
    const shape = await page.evaluate(() => {
      const titled = Array.from(document.querySelectorAll<HTMLElement>('[title]'));
      const bands = titled.filter((el) =>
        getComputedStyle(el).backgroundImage.includes('135deg'),
      );
      const atTrackLeft = (el: HTMLElement) => {
        const track = el.parentElement?.getBoundingClientRect();
        return track ? el.getBoundingClientRect().left - track.left < 2 : false;
      };
      return {
        titled: titled.length,
        headBands: bands.filter(atTrackLeft).length, // truncated (flush left)
        tailBands: bands.filter((el) => !atTrackLeft(el)).length, // offline
      };
    });
    // A non-zero `tailBands` doubles as a load-bearing guard that Tailwind is
    // actually applied: the offline band only sits away from the track's left
    // edge if `absolute` + `left: N%` resolved. If the stylesheet were missing
    // (as it silently was under a cold CI build until `@source` was pinned),
    // every element stacks at offset 0 and this fails instead of running the
    // hit-testing assertion against an unstyled tree where it means nothing.
    expect(shape.titled, 'fixture should render several titled elements').toBeGreaterThan(4);
    expect(shape.headBands, 'fixture should render a truncated (head) band').toBeGreaterThan(0);
    expect(shape.tailBands, 'fixture should render an offline (tail) band').toBeGreaterThan(0);

    // The invariant: a written tooltip must be able to appear. For every
    // element carrying a `title`, hovering its centre must resolve to a
    // tooltip-bearing element — itself, or another titled element legitimately
    // above it (a 1px marker). If `elementFromPoint` lands on something with no
    // title in its ancestry, that point is dead: the tooltip was authored,
    // reviewed and shipped, yet can never fire. That is exactly the
    // `pointer-events-none`-on-the-band bug from #1088.
    const unreachable = await page.evaluate(() => {
      const bad: { title: string; rect: number[] }[] = [];
      for (const el of Array.from(document.querySelectorAll<HTMLElement>('[title]'))) {
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) continue;
        const cx = r.left + r.width / 2;
        const cy = r.top + r.height / 2;
        const hit = document.elementFromPoint(cx, cy) as HTMLElement | null;
        if (!hit || !hit.closest('[title]')) {
          bad.push({
            title: el.getAttribute('title') ?? '',
            rect: [r.left, r.top, r.width, r.height],
          });
        }
      }
      return bad;
    });
    expect(unreachable, 'these titled elements cannot receive a hover').toEqual([]);
  });
});

test.describe('OperationalTimeline non-overlap (no plaid)', () => {
  test('no lane paints a no-evidence band over a span', async ({ mount, page }) => {
    await mount(
      <div style={{ width: 900 }}>
        <OperationalTimeline
          events={EVENTS}
          from={FROM}
          to={TO}
          coverageFrom={COVERAGE_FROM}
          lastHeartbeat={LAST_HEARTBEAT}
        />
      </div>,
    );

    // Issue #1088's plaid: the no-evidence band (135° grey hatch, transparent
    // gaps) drawn UNDER an unconfirmed span (45° coloured hatch, also gapped)
    // — where they share an x-range, each shows through the other's gaps and
    // the stretch asserts two meanings at once ("unknown" AND "believed on").
    // The fix is `subtractRanges(noEvidence, lane.spans)`: the band is only
    // drawn in the gaps a lane's spans leave, so band and span never coincide.
    // No DOM assertion in jsdom can see a compositing overlap — this needs a
    // laid-out browser, which is the whole reason this file exists.
    //
    // The invariant, per lane track: no 135° band's x-range may overlap any
    // fill span's (solid OR 45° hatch). Markers are excluded — they are 1px
    // transition lines that legitimately sit over a span, not a conflicting
    // fill. The fixture's power/session lanes each have an unconfirmed hatch
    // tail in the same stretch the offline band would cover, so dropping the
    // subtraction brings the plaid straight back here.
    const conflicts = await page.evaluate(() => {
      const classify = (el: HTMLElement): 'band' | 'hatch' | 'marker' | 'solid' => {
        const bg = getComputedStyle(el).backgroundImage;
        if (bg.includes('135deg')) return 'band';
        if (bg.includes('45deg')) return 'hatch';
        return el.getBoundingClientRect().width <= 2 ? 'marker' : 'solid';
      };
      // Lane tracks are the parents of the titled span/band/marker elements.
      const tracks = new Set<HTMLElement>();
      for (const el of Array.from(document.querySelectorAll<HTMLElement>('[title]'))) {
        if (el.parentElement) tracks.add(el.parentElement);
      }
      const bad: { band: string; fill: string; overlapPx: number }[] = [];
      for (const track of tracks) {
        const kids = Array.from(track.children).filter(
          (el): el is HTMLElement => el instanceof HTMLElement && el.hasAttribute('title'),
        );
        const items = kids.map((el) => ({ el, kind: classify(el), r: el.getBoundingClientRect() }));
        const bands = items.filter((i) => i.kind === 'band');
        const fills = items.filter((i) => i.kind === 'solid' || i.kind === 'hatch');
        for (const b of bands) {
          for (const f of fills) {
            // >1px, so touching edges / sub-pixel anti-aliasing aren't flagged.
            const overlap = Math.min(b.r.right, f.r.right) - Math.max(b.r.left, f.r.left);
            if (overlap > 1) {
              bad.push({
                band: (b.el.getAttribute('title') ?? '').slice(0, 40),
                fill: (f.el.getAttribute('title') ?? '').slice(0, 40),
                overlapPx: Math.round(overlap),
              });
            }
          }
        }
      }
      return bad;
    });
    expect(conflicts, 'a no-evidence band overlaps a span (the #1088 plaid)').toEqual([]);
  });
});
