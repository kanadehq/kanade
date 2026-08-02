import { expect, test } from '@playwright/experimental-ct-react';

import { WidgetCard, type BarRow, type Widget } from './AnalyticsWidget';

// Issue #1259: the pie/donut widgets showed a legend with no pie behind it.
// recharts' mount animation starts every sector at `startAngle ===
// endAngle`, and `Sector` returns null at zero width, so the
// `recharts-pie-sector` groups sat in the DOM empty.
//
// What stalled the animation was the page being HIDDEN: Chrome does not run
// `requestAnimationFrame` in a background tab, so react-smooth never
// advanced past frame 0 and the arcs never appeared — a dashboard left open
// in a background tab drew legends over nothing until it was focused.
//
// These count arcs WITHOUT Playwright's auto-retry (`locator.count()`, not
// `toHaveCount`), on purpose: the property under test is that the arcs are
// there on first paint rather than at the end of an animation. An assertion
// that retried for 5s would wait the animation out and pass either way —
// which is exactly what the first version of this test did.

const t = (k: string) => k;

const pie = (rows: BarRow[], donut: boolean): Widget => ({
  dashboard: 'ct',
  title: 'OS versions',
  scope: 'fleet',
  render: 'pie',
  rows,
  donut,
});

const ROWS = [
  { label: 'Windows 11 Pro 25H2', value: 231 },
  { label: 'Windows 11 Pro 24H2', value: 8 },
  { label: 'Ubuntu 24.04.1 LTS', value: 8 },
  { label: 'Windows 11 Pro 23H2', value: 1 },
];

test.describe('AnalyticsWidget pie', () => {
  test('a donut draws one arc per row on first paint', async ({ mount }) => {
    const c = await mount(<WidgetCard w={pie(ROWS, true)} t={t} />);
    expect(await c.locator('path.recharts-sector').count()).toBe(ROWS.length);
    // The centre total is the donut-only overlay, aligned to the pie centre.
    await expect(c.getByText('248')).toBeVisible();
  });

  test('a plain pie draws its arcs on first paint', async ({ mount }) => {
    const c = await mount(<WidgetCard w={pie(ROWS, false)} t={t} />);
    expect(await c.locator('path.recharts-sector').count()).toBe(ROWS.length);
  });

  test('a single-row pie still draws its one arc', async ({ mount }) => {
    // `paddingAngle` drops to 0 below two rows; guard that the lone
    // full-circle sector is not the case that collapses.
    const c = await mount(<WidgetCard w={pie([{ label: 'only', value: 5 }], false)} t={t} />);
    expect(await c.locator('path.recharts-sector').count()).toBe(1);
  });

  // The no-data branch is deliberately not covered here: CT serialises the
  // props it mounts with, and a `rows: []` widget arrives broken enough that
  // the card renders nothing at all — for `bar` just as much as for `pie`,
  // so it is the empty array, not the component. That branch is plain
  // conditional text and belongs in the unit suite anyway.
});
