import { expect, test } from '@playwright/experimental-ct-react';

import { PlainTable, SortFilterTable } from './table.ct.harness';

// Excel-style sorting / filtering is opt-in (`<Table sortFilter>`). What is
// worth asserting here only exists in a real browser: the portalled popover
// over the sticky <thead>, the card-mode fallback, and that a table which
// did not opt in is untouched.

const DESKTOP = { width: 1280, height: 720 };
const NARROW = { width: 800, height: 720 };

type Texts = { allTextContents(): Promise<string[]> };
type Clickable = { click(): Promise<void> };

const NAMES = ['banana', 'apple', 'cherry', 'carrot'];

async function names(c: { locator(sel: string): Texts }): Promise<string[]> {
  return c.locator('tbody tr td:first-child').allTextContents();
}

async function openFilter(c: { getByRole(role: 'button', o: { name: string }): Clickable }, column: string) {
  await c.getByRole('button', { name: `Filter ${column}` }).click();
}

test.describe('Table sort and filter', () => {
  test.use({ viewport: DESKTOP });

  test('a table that did not opt in grows no sort or filter controls', async ({ mount }) => {
    const c = await mount(<PlainTable />);
    await expect(c.locator('thead button')).toHaveCount(0);
    await expect(c.locator('thead [aria-sort]')).toHaveCount(0);
  });

  test('clicking a header cycles ascending, descending, off', async ({ mount }) => {
    const c = await mount(<SortFilterTable />);
    const th = c.locator('th[data-col-id="name"]');
    const button = th.getByRole('button', { name: 'name', exact: true });
    await expect(th).toHaveAttribute('aria-sort', 'none');

    await button.click();
    await expect(th).toHaveAttribute('aria-sort', 'ascending');
    expect(await names(c)).toEqual(['apple', 'banana', 'carrot', 'cherry']);

    await button.click();
    await expect(th).toHaveAttribute('aria-sort', 'descending');
    expect(await names(c)).toEqual(['cherry', 'carrot', 'banana', 'apple']);

    await button.click();
    await expect(th).toHaveAttribute('aria-sort', 'none');
    expect(await names(c)).toEqual(NAMES);
  });

  test('sorts by sortValue, numerically, with blanks last both ways', async ({ mount }) => {
    const c = await mount(<SortFilterTable />);
    const button = c.locator('th[data-col-id="size"]').getByRole('button', { name: 'size', exact: true });
    await button.click();
    expect(await names(c)).toEqual(['banana', 'apple', 'cherry', 'carrot']); // 9, 10, 100, blank
    await button.click();
    expect(await names(c)).toEqual(['cherry', 'apple', 'banana', 'carrot']); // 100, 10, 9, blank
  });

  test('columns that opt out get neither a sort button nor a filter', async ({ mount }) => {
    const c = await mount(<SortFilterTable />);
    const note = c.locator('th[data-col-id="note"]');
    await expect(note.getByRole('button')).toHaveCount(0);
    await expect(note).not.toHaveAttribute('aria-sort', /.*/);
  });

  test('the sort is written to localStorage', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await c.locator('th[data-col-id="name"]').getByRole('button', { name: 'name', exact: true }).click();
    const stored = await page.evaluate(() => localStorage.getItem('kanade.table.sort.ct-sf'));
    expect(JSON.parse(stored!)).toEqual({ id: 'name', dir: 'asc' });

  });

  test('a stored sort is applied on first render', async ({ mount, page }) => {
    await page.evaluate(() => localStorage.setItem('kanade.table.sort.ct-sf', '{"id":"name","dir":"desc"}'));
    const c = await mount(<SortFilterTable />);
    expect(await names(c)).toEqual(['cherry', 'carrot', 'banana', 'apple']);
  });

  test('a stored sort for a missing column is ignored', async ({ mount, page }) => {
    await page.evaluate(() => localStorage.setItem('kanade.table.sort.ct-sf', '{"id":"gone","dir":"asc"}'));
    const c = await mount(<SortFilterTable />);
    expect(await names(c)).toEqual(NAMES);
  });

  test('a corrupt stored sort is ignored', async ({ mount, page }) => {
    await page.evaluate(() => localStorage.setItem('kanade.table.sort.ct-sf', '{"id":"name","dir":"sideways"}'));
    const c = await mount(<SortFilterTable />);
    expect(await names(c)).toEqual(NAMES);
  });

  test('contains filter narrows the rows and shows a chip', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await openFilter(c, 'name');
    await page.getByRole('searchbox', { name: 'name contains' }).fill('AN');
    expect(await names(c)).toEqual(['banana']);
    await expect(c.getByTestId('filter-chip')).toContainText('name: "AN"');
    await expect(c.getByTestId('filter-count')).toHaveText('1 / 4 rows');
    // The funnel is marked while a filter is active.
    await expect(c.locator('th[data-col-id="name"] [data-filter-active]')).toHaveCount(1);
  });

  test('the value checklist filters, and select-all restores', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await openFilter(c, 'kind');
    const p = page;
    await p.getByRole('checkbox', { name: /^fruit/ }).uncheck();
    expect(await names(c)).toEqual(['carrot']);
    await p.getByRole('checkbox', { name: /Select all/ }).check();
    expect(await names(c)).toEqual(NAMES);
    await expect(c.getByTestId('filter-chip')).toHaveCount(0);
  });

  test('the popover is not clipped by the sticky header', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await expect(c.locator('thead')).toHaveCSS('position', 'sticky');
    await openFilter(c, 'kind');
    const dialog = page.getByRole('dialog');
    await expect(dialog).toBeVisible();
    // Hit-test the middle of the panel: it must be the panel, not the thead
    // (z-20) or a row, painting there.
    const box = await dialog.boundingBox();
    const top = await page.evaluate(
      ([x, y]) => !!document.elementFromPoint(x, y)?.closest('[role="dialog"]'),
      [box!.x + box!.width / 2, box!.y + box!.height / 2],
    );
    expect(top).toBe(true);
  });

  test('Escape closes the popover', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await openFilter(c, 'kind');
    await expect(page.getByRole('dialog')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog')).toHaveCount(0);
  });

  test('columns combine with AND, and each chip clears its own column', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    const p = page;
    await openFilter(c, 'kind');
    await p.getByRole('checkbox', { name: /^veg/ }).uncheck(); // fruit only
    await page.keyboard.press('Escape');
    await openFilter(c, 'name');
    await p.getByRole('searchbox', { name: 'name contains' }).fill('an');
    await page.keyboard.press('Escape');
    expect(await names(c)).toEqual(['banana']);
    await expect(c.getByTestId('filter-chip')).toHaveCount(2);

    await c.getByRole('button', { name: 'Remove the filter on name' }).click();
    expect(await names(c)).toEqual(['banana', 'apple', 'cherry']);
  });

  test('clear all drops every filter', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    const p = page;
    await openFilter(c, 'name');
    await p.getByRole('searchbox', { name: 'name contains' }).fill('a');
    await page.keyboard.press('Escape');
    await openFilter(c, 'kind');
    await p.getByRole('checkbox', { name: /^veg/ }).uncheck();
    await page.keyboard.press('Escape');
    await c.getByRole('button', { name: 'Clear all filters' }).click();
    expect(await names(c)).toEqual(NAMES);
    await expect(c.getByTestId("table-filter-bar")).toHaveCount(0);
  });

  test('no match shows an empty state and keeps the header', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await openFilter(c, 'name');
    await page.getByRole('searchbox', { name: 'name contains' }).fill('zzz');
    await expect(c.getByText('No rows match the filters.')).toBeVisible();
    await expect(c.locator('th[data-col-id="name"]')).toBeVisible();
  });

  test('filters are not persisted', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await openFilter(c, 'name');
    await page.getByRole('searchbox', { name: 'name contains' }).fill('an');
    const keys = await page.evaluate(() => Object.keys(localStorage));
    expect(keys.filter((k) => k.startsWith('kanade.table.filters'))).toEqual([]);
  });

  test('rows that are not data rows stay where they were', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable pinned />);
    await c.locator('th[data-col-id="name"]').getByRole('button', { name: 'name', exact: true }).click();
    await openFilter(c, 'name');
    await page.getByRole('searchbox', { name: 'name contains' }).fill('an');
    const rows = await c.locator('tbody tr').allTextContents();
    expect(rows[0]).toContain('group header');
    expect(rows[rows.length - 1]).toContain('detail row');
    expect(rows).toHaveLength(3);
  });

  test('a stored column order does not misapply a filter', async ({ mount, page }) => {
    await page.evaluate(() =>
      localStorage.setItem('kanade.table.order.ct-sf', JSON.stringify(['kind', 'name', 'size', 'note'])),
    );
    const c = await mount(<SortFilterTable />);
    await openFilter(c, 'name');
    await page.getByRole('searchbox', { name: 'name contains' }).fill('ap');
    await expect(c.locator('tbody tr')).toHaveCount(1);
    await expect(c.locator('tbody tr')).toContainText('apple');
  });

  test('a filter on a column the picker then hides stays visible as a chip', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await openFilter(c, 'kind');
    await page.getByRole('checkbox', { name: /^veg/ }).uncheck();
    await page.keyboard.press('Escape');
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: 'kind' }).uncheck();
    await expect(c.getByTestId('filter-chip')).toContainText('kind');
    expect(await names(c)).toEqual(['banana', 'apple', 'cherry']);
  });

  test('resize handles are unaffected', async ({ mount }) => {
    const c = await mount(<SortFilterTable />);
    await expect(c.getByRole('separator')).toHaveCount(4);
  });

  test('the page note shows only for server-paged tables', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable page />);
    await openFilter(c, 'name');
    await page.getByRole('searchbox', { name: 'name contains' }).fill('an');
    await expect(c.getByText('Applies to the rows on this page only.')).toBeVisible();
  });
});

test.describe('Table sort and filter in card mode', () => {
  test.use({ viewport: NARROW });

  test('a menu stands in for the hidden header', async ({ mount, page }) => {
    const c = await mount(<SortFilterTable />);
    await expect(c.locator('thead')).toBeHidden();
    await c.getByRole('button', { name: 'Sort / filter' }).click();
    await page.getByRole('dialog').locator('select').selectOption('kind');
    await page.getByRole('checkbox', { name: /^fruit/ }).uncheck();
    await expect(c.getByTestId('filter-chip')).toContainText('kind');
    await expect(c.locator('tbody tr')).toHaveCount(1);
  });

  test('a table that did not opt in has no menu', async ({ mount }) => {
    const c = await mount(<PlainTable />);
    await expect(c.getByRole('button', { name: 'Sort / filter' })).toHaveCount(0);
  });
});
