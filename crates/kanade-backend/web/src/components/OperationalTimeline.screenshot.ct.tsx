import { expect, test } from '@playwright/experimental-ct-react';

import { OperationalTimeline } from './OperationalTimeline';

// Issue #1094, third increment: screenshot baselines. Two of the shipped bugs
// were perceptual (a sub-pixel band; grey-vs-blue hatch at low alpha) — not
// automatable as pass/fail, but a baseline makes any *change* to them show up
// in review instead of slipping by. Kept deliberately small (one rich state ×
// light/dark); the baselines are generated in CI (Linux) and committed, never
// generated locally — see `snapshotPathTemplate` in playwright-ct.config.ts.
//
// Unlike the hit-testing / non-overlap tests, these MUST be pixel-stable, so
// the clock is PINNED rather than relative-to-now. A fixed instant fixes both
// the component's internal `Date.now()` (the live / evidence edges) AND the
// axis tick labels the window derives from `from`/`to` — a relative window
// would render different times every run and no baseline could ever match.
const FIXED = '2026-07-19T20:00:00.000Z';
const F0 = Date.parse(FIXED);
const HOUR = 3_600_000;
const at = (deltaMs: number) => new Date(F0 + deltaMs).toISOString();

// The same rich state the other tests exercise: solid spans, an unconfirmed
// hatch tail, transition markers, and truncated + offline no-evidence bands.
const FIXTURE = {
  events: [
    { at: at(-18 * HOUR), kind: 'boot' },
    { at: at(-17.5 * HOUR), kind: 'logon' },
    { at: at(-12 * HOUR), kind: 'lock' },
    { at: at(-11 * HOUR), kind: 'unlock' },
    { at: at(-8 * HOUR), kind: 'sleep' },
    { at: at(-7 * HOUR), kind: 'resume' },
  ],
  from: at(-20 * HOUR),
  to: at(-60_000),
  coverageFrom: at(-19 * HOUR),
  lastHeartbeat: at(-6 * HOUR),
};

test.describe('OperationalTimeline screenshots', () => {
  // Only Linux baselines are committed (they're generated in CI). On a dev's
  // win32/darwin box `toHaveScreenshot` would look for a `-win32`/`-darwin`
  // baseline that doesn't exist and fail `bun run test:ct` — so skip the whole
  // suite off Linux. The hit-testing / non-overlap tests still run everywhere;
  // only the pixel baselines are platform-locked.
  test.skip(
    process.platform !== 'linux',
    'Screenshot baselines are committed for Linux (CI) only; see playwright-ct.config.ts',
  );

  for (const scheme of ['light', 'dark'] as const) {
    test(`swimlane rich state — ${scheme}`, async ({ mount, page }) => {
      // Theme comes from `@media (prefers-color-scheme)` in index.css, so
      // emulating it is enough — no need to toggle the app's `.dark` class.
      await page.emulateMedia({ colorScheme: scheme });
      // Pin the clock BEFORE mount so the component's first render already
      // reads the fixed instant.
      await page.clock.setFixedTime(new Date(FIXED));
      const component = await mount(
        <div className="bg-card text-fg p-4" style={{ width: 900 }}>
          <OperationalTimeline {...FIXTURE} />
        </div>,
      );
      await expect(component).toHaveScreenshot(`swimlane-rich-${scheme}.png`);
    });
  }
});
