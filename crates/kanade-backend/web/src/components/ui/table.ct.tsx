import { expect, test } from '@playwright/experimental-ct-react';

import { NonCardResizableTable, PlainTable, ResizableTable, TableWithExternalReset } from './table.ct.harness';

// #1344: column resizing is a pure layout feature — every property worth
// asserting (does the column actually get wider, does its neighbour stay
// put, does the wrapper still contain the table, does the card layout stay
// free of the desktop width) only exists once a real browser has laid the
// table out. That is what CT is for here (Issue #1094); `bun test` can't
// see any of it.

const DESKTOP = { width: 1280, height: 720 };
/** Below the `lg` card breakpoint, where the table becomes stacked cards. */
const NARROW = { width: 800, height: 720 };

// Structural stand-ins for Locator / Page: `@playwright/experimental-ct-react`
// re-exports the runner but not the core types, and `@playwright/test` is not
// a direct dependency of this package.
type Measurable = { boundingBox(): Promise<{ x: number; y: number; width: number; height: number } | null> };
type WithMouse = {
  mouse: {
    move(x: number, y: number, options?: { steps?: number }): Promise<void>;
    down(): Promise<void>;
    up(): Promise<void>;
  };
};

async function widthOf(cell: Measurable): Promise<number> {
  const box = await cell.boundingBox();
  if (!box) throw new Error('cell has no box');
  return box.width;
}

/** Drag a resize handle `dx` px horizontally. */
async function drag(page: WithMouse, handle: Measurable, dx: number) {
  const box = await handle.boundingBox();
  if (!box) throw new Error('handle has no box');
  const y = box.y + box.height / 2;
  const x = box.x + box.width / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  // Stepped so pointermove fires more than once, as a real drag does.
  await page.mouse.move(x + dx, y, { steps: 8 });
  await page.mouse.up();
}

test.describe('Table column resizing', () => {
  test.use({ viewport: DESKTOP });

  test('a table that did not opt in has no handles at all', async ({ mount }) => {
    const c = await mount(<PlainTable />);
    await expect(c.getByRole('separator')).toHaveCount(0);
    await expect(c.locator('colgroup')).toHaveCount(0);
  });

  test('an untouched table keeps automatic layout', async ({ mount, page }) => {
    const c = await mount(<ResizableTable />);
    // Handles are there to grab...
    await expect(c.getByRole('separator')).toHaveCount(4);
    // ...but nothing is pinned until the operator drags: no <colgroup>, no
    // fixed layout, no stored widths. A regression here would change how
    // every table in the app sizes its columns on first paint.
    await expect(c.locator('colgroup')).toHaveCount(0);
    await expect(c.locator('table')).toHaveCSS('table-layout', 'auto');
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.widths.ct'))).toBeNull();
  });

  test('dragging widens the grabbed column and leaves its neighbour alone', async ({ mount, page }) => {
    const c = await mount(<ResizableTable />);
    const alpha = c.locator('th', { hasText: 'alpha' });
    const bravo = c.locator('th', { hasText: 'bravo' });
    const before = { alpha: await widthOf(alpha), bravo: await widthOf(bravo) };

    await drag(page, alpha.getByRole('separator'), 120);

    // The whole point of measuring the other columns before switching to
    // fixed layout: the drag moves ONE column, it doesn't redistribute the
    // rest.
    expect(await widthOf(alpha)).toBeCloseTo(before.alpha + 120, -1);
    expect(await widthOf(bravo)).toBeCloseTo(before.bravo, -1);
    await expect(c.locator('colgroup')).toHaveCount(1);
    await expect(c.locator('table')).toHaveCSS('table-layout', 'fixed');
  });

  test('a widened table grows its wrapper instead of escaping the card border', async ({ mount, page }) => {
    const c = await mount(<ResizableTable />);
    const alpha = c.locator('th', { hasText: 'alpha' });
    // Far past the container — #1005 left the wrapper with no horizontal
    // scroll, so if the wrapper didn't grow with it the table would render
    // outside the rounded border and only the page would scroll.
    await drag(page, alpha.getByRole('separator'), 900);

    // The mounted root IS the wrapper — <Table> renders it.
    await expect(c).toHaveClass(/kn-table/);
    const wrapperBox = await c.boundingBox();
    const tableBox = await c.locator('table').boundingBox();
    expect(wrapperBox).not.toBeNull();
    expect(tableBox).not.toBeNull();
    expect(wrapperBox!.width).toBeGreaterThanOrEqual(tableBox!.width);
    // And it really did get wider than the viewport gave it.
    expect(tableBox!.width).toBeGreaterThan(DESKTOP.width);
  });

  test('narrowing the columns shrinks the card with them', async ({ mount, page }) => {
    // The card draws a border around the table, so it has to end where the
    // table ends. Left at `min-width: 100%` it stayed stretched to the full
    // width and put dead space inside the border, to the right of the last
    // column — which reads as a broken layout, not as a narrow table.
    const c = await mount(<ResizableTable />);
    const before = (await c.boundingBox())!.width;
    await drag(page, c.locator('th', { hasText: 'alpha' }).getByRole('separator'), -200);

    const wrapper = (await c.boundingBox())!.width;
    const table = (await c.locator('table').boundingBox())!.width;
    expect(wrapper).toBeLessThan(before);
    // Border and content end together — no gap either side of the table.
    expect(wrapper).toBeCloseTo(table, -1);
  });

  test('a column cannot be dragged narrower than the minimum', async ({ mount, page }) => {
    const c = await mount(<ResizableTable />);
    const alpha = c.locator('th', { hasText: 'alpha' });
    await drag(page, alpha.getByRole('separator'), -900);
    expect(await widthOf(alpha)).toBeCloseTo(56, -1);
  });

  test('widths persist and come back on the next mount', async ({ mount, page }) => {
    const first = await mount(<ResizableTable />);
    const alpha = first.locator('th', { hasText: 'alpha' });
    await drag(page, alpha.getByRole('separator'), 120);
    const widened = await widthOf(alpha);

    const stored = await page.evaluate(() => localStorage.getItem('kanade.table.widths.ct'));
    expect(stored).not.toBeNull();
    expect(JSON.parse(stored!).alpha).toBeCloseTo(widened, -1);

    await first.unmount();
    const second = await mount(<ResizableTable />);
    expect(await widthOf(second.locator('th', { hasText: 'alpha' }))).toBeCloseTo(widened, -1);
  });

  test('a column re-added after the widths were stored gets a real width', async ({ mount, page }) => {
    // What a column picker does: resize with `charlie` hidden, then show it
    // again. Under fixed layout a column with no stored width renders at
    // zero, so it has to be measured and folded in.
    const first = await mount(<ResizableTable hide="charlie" />);
    await drag(page, first.locator('th', { hasText: 'alpha' }).getByRole('separator'), 80);
    await first.unmount();

    const second = await mount(<ResizableTable />);
    const charlie = second.locator('th', { hasText: 'charlie' });
    await expect(charlie).toBeVisible();
    expect(await widthOf(charlie)).toBeGreaterThanOrEqual(56);
  });

  test('double-clicking a handle resets every column to automatic', async ({ mount, page }) => {
    const c = await mount(<ResizableTable />);
    const alpha = c.locator('th', { hasText: 'alpha' });
    const original = await widthOf(alpha);
    await drag(page, alpha.getByRole('separator'), 120);
    expect(await widthOf(alpha)).toBeGreaterThan(original + 100);

    await alpha.getByRole('separator').dblclick();
    await expect(c.locator('colgroup')).toHaveCount(0);
    await expect(c.locator('table')).toHaveCSS('table-layout', 'auto');
    expect(await widthOf(alpha)).toBeCloseTo(original, -1);
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.widths.ct'))).toBeNull();
  });

  test('arrow keys on a focused handle resize the column', async ({ mount }) => {
    const c = await mount(<ResizableTable />);
    const alpha = c.locator('th', { hasText: 'alpha' });
    const original = await widthOf(alpha);
    await alpha.getByRole('separator').focus();
    await alpha.getByRole('separator').press('ArrowRight');
    await alpha.getByRole('separator').press('ArrowRight');
    expect(await widthOf(alpha)).toBeCloseTo(original + 32, -1);
  });

  test('a reset control outside the table appears only after a resize, and clears it', async ({ mount, page }) => {
    const c = await mount(<TableWithExternalReset />);
    const alpha = c.locator('th', { hasText: 'alpha' });
    const original = await widthOf(alpha);
    // Nothing resized yet ⇒ the control isn't rendered at all, rather than
    // sitting there dead. This is what `hasWidths` is for.
    await expect(c.getByRole('button', { name: 'reset widths' })).toHaveCount(0);

    await drag(page, alpha.getByRole('separator'), 120);
    // The button is outside the table, so it only knows about the drag
    // through the shared width store.
    await expect(c.getByRole('button', { name: 'reset widths' })).toBeVisible();

    await c.getByRole('button', { name: 'reset widths' }).click();
    await expect(c.locator('colgroup')).toHaveCount(0);
    expect(await widthOf(alpha)).toBeCloseTo(original, -1);
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.widths.ct'))).toBeNull();
  });

  test('the global reset clears tables that are not mounted', async ({ mount, page }) => {
    // A table the operator squeezed on some other page: its widths exist
    // only in localStorage, so a reset that walked the mounted tables would
    // silently miss it — the exact case this control is for.
    await page.evaluate(() =>
      localStorage.setItem('kanade.table.widths.somewhere-else', JSON.stringify({ a: 300 })),
    );
    const c = await mount(<TableWithExternalReset />);
    await drag(page, c.locator('th', { hasText: 'alpha' }).getByRole('separator'), 120);

    await c.getByRole('button', { name: 'reset all' }).click();
    await expect(c.locator('colgroup')).toHaveCount(0);
    expect(
      await page.evaluate(() => [
        localStorage.getItem('kanade.table.widths.ct'),
        localStorage.getItem('kanade.table.widths.somewhere-else'),
      ]),
    ).toEqual([null, null]);
  });

  test('a cards={false} table scrolls inside its wrapper instead of growing it', async ({ mount, page }) => {
    // That wrapper is already a horizontal scroll container. Growing it to
    // `max-content` (what a `cards` table does) would defeat its own
    // `overflow-x-auto` and push it past its parent, so the sizing style is
    // deliberately not applied here.
    const c = await mount(<NonCardResizableTable />);
    const wrapperBefore = (await c.boundingBox())!.width;
    await drag(page, c.locator('th', { hasText: 'alpha' }).getByRole('separator'), 700);

    await expect(c.locator('colgroup')).toHaveCount(1);
    const wrapperAfter = (await c.boundingBox())!.width;
    const tableAfter = (await c.locator('table').boundingBox())!.width;
    expect(wrapperAfter).toBeCloseTo(wrapperBefore, -1);
    expect(tableAfter).toBeGreaterThan(wrapperAfter);
    // The overflow really is the wrapper's to scroll, not the page's.
    expect(await c.evaluate((el) => el.scrollWidth > el.clientWidth)).toBe(true);
  });

  test('card mode drops the handles and never inherits the desktop width', async ({ mount, page }) => {
    const c = await mount(<ResizableTable />);
    await drag(page, c.locator('th', { hasText: 'alpha' }).getByRole('separator'), 400);

    await page.setViewportSize(NARROW);
    // Below `lg` the rows are stacked cards: an inline width or a
    // <colgroup> left over from desktop would out-specify the card CSS and
    // blow every card out to the table's width.
    await expect(c.locator('colgroup')).toHaveCount(0);
    await expect(c.getByRole('separator')).toHaveCount(0);
    const tableBox = await c.locator('table').boundingBox();
    expect(tableBox!.width).toBeLessThanOrEqual(NARROW.width);

    // ...and the widths are not lost — they come back when it is a table again.
    await page.setViewportSize(DESKTOP);
    await expect(c.locator('colgroup')).toHaveCount(1);
  });
});
