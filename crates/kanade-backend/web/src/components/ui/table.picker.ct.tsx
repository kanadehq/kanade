import { expect, test } from '@playwright/experimental-ct-react';

import { LonePicker, TableWithForcedToggle, TableWithPicker } from './table.ct.harness';

// #1353 phase 1: column visibility. Hiding is done by a positional
// stylesheet the table emits, not by declining to render the cell, so
// every property worth asserting is about what the browser actually laid
// out — which is what CT is for here (Issue #1094).

const DESKTOP = { width: 1280, height: 720 };
/** Below the `lg` card breakpoint, where rows become stacked cards. */
const NARROW = { width: 800, height: 720 };

test.describe('Table column picker', () => {
  test.use({ viewport: DESKTOP });

  test('lists the live header as its columns, with no props from the page', async ({ mount }) => {
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    // The names are the header cells' own text — the page passed none.
    for (const name of ['alpha', 'bravo', 'charlie', 'delta']) {
      await expect(c.getByRole('checkbox', { name })).toBeChecked();
    }
  });

  test('renders nothing when no table has published a header', async ({ mount }) => {
    // A picker whose table isn't mounted has nothing to offer, and a dead
    // control is worse than no control.
    const c = await mount(<LonePicker />);
    await expect(c.locator('summary')).toHaveCount(0);
  });

  test('a table that has unmounted leaves no columns behind', async ({ mount }) => {
    // The registry is what the picker reads. If a table's entry outlived
    // it, a picker mounted for the same key — a standalone one, or a route
    // that renders the picker before the table — would offer the previous
    // mount's columns and toggle them against nothing.
    const table = await mount(<TableWithPicker />);
    await expect(table.locator('summary')).toHaveCount(1);
    await table.unmount();

    const lone = await mount(<LonePicker resizeKey="ct-pick" />);
    await expect(lone.locator('summary')).toHaveCount(0);
  });

  test('unchecking a column removes it from the header and every row', async ({ mount, page }) => {
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: 'bravo' }).uncheck();

    await expect(c.locator('th', { hasText: 'bravo' })).toBeHidden();
    await expect(c.locator('td', { hasText: 'bravo value' })).toBeHidden();
    // Its neighbours are untouched — a positional rule that was off by one
    // would take the wrong column with it.
    await expect(c.locator('th', { hasText: 'alpha' })).toBeVisible();
    await expect(c.locator('th', { hasText: 'charlie' })).toBeVisible();
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.hidden.ct-pick'))).toBe(
      JSON.stringify(['bravo']),
    );
  });

  test('an empty-state colSpan row survives hiding the first column', async ({ mount }) => {
    // The regression this guards: "no results" is ONE cell spanning the
    // table, so it is `:nth-child(1)`. Without the `:not(:has(> [colspan]))`
    // guard, hiding the first column deletes the message instead.
    const c = await mount(<TableWithPicker empty />);
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: 'alpha' }).uncheck();

    await expect(c.locator('th', { hasText: 'alpha' })).toBeHidden();
    await expect(c.getByText('nothing here')).toBeVisible();
  });

  test('the last visible column cannot be hidden', async ({ mount }) => {
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    for (const name of ['alpha', 'bravo', 'charlie']) {
      await c.getByRole('checkbox', { name }).uncheck();
    }
    // Nothing left to uncheck it FROM — the picker lists columns, so a
    // table with none has no way back.
    await expect(c.getByRole('checkbox', { name: 'delta' })).toBeDisabled();
    await expect(c.locator('th', { hasText: 'delta' })).toBeVisible();
  });

  test('the store itself refuses to hide the last column', async ({ mount, page }) => {
    // The picker disables that checkbox, so this invariant is unreachable
    // through the UI — but `useTableColumns` is exported for pages to build
    // their own controls, and it has to hold there too.
    const c = await mount(<TableWithForcedToggle />);
    for (const id of ['alpha', 'bravo', 'charlie', 'delta']) {
      await c.getByRole('button', { name: `toggle ${id}` }).click();
    }
    await expect(c.locator('th', { hasText: 'delta' })).toBeVisible();
    const stored = await page.evaluate(() => localStorage.getItem('kanade.table.hidden.ct-force'));
    expect(JSON.parse(stored!)).toEqual(['alpha', 'bravo', 'charlie']);
  });

  test('two toggles in one tick cannot empty the table between them', async ({ mount, page }) => {
    // React batches the re-render, so both calls see the same store value
    // going in. The guard has to be decided from `prev` inside the updater,
    // not from a visible-count captured when the callback was built.
    const c = await mount(<TableWithForcedToggle />);
    await c.getByRole('button', { name: 'toggle alpha' }).click();
    await c.getByRole('button', { name: 'toggle bravo' }).click();
    await c.getByRole('button', { name: 'toggle last two at once' }).click();

    await expect(c.locator('th', { hasText: 'delta' })).toBeVisible();
    const stored = await page.evaluate(() => localStorage.getItem('kanade.table.hidden.ct-force'));
    expect(JSON.parse(stored!)).toEqual(['alpha', 'bravo', 'charlie']);
  });

  test('hiding persists and comes back on the next mount', async ({ mount, page }) => {
    const first = await mount(<TableWithPicker />);
    await first.locator('summary').click();
    await first.getByRole('checkbox', { name: 'charlie' }).uncheck();
    await first.unmount();

    const second = await mount(<TableWithPicker />);
    await expect(second.locator('th', { hasText: 'charlie' })).toBeHidden();
    await expect(second.locator('th', { hasText: 'alpha' })).toBeVisible();
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.hidden.ct-pick'))).toBe(
      JSON.stringify(['charlie']),
    );
  });

  test('"show all" restores every column and clears the stored set', async ({ mount, page }) => {
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: 'bravo' }).uncheck();
    await expect(c.locator('th', { hasText: 'bravo' })).toBeHidden();

    await c.getByRole('button', { name: 'Show all columns' }).click();
    await expect(c.locator('th', { hasText: 'bravo' })).toBeVisible();
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.hidden.ct-pick'))).toBeNull();
    // The control removes itself once there is nothing left to restore.
    await expect(c.getByRole('button', { name: 'Show all columns' })).toHaveCount(0);
  });

  test('a hidden column is gone in card mode too', async ({ mount, page }) => {
    // Below the breakpoint the row is a stack of label:value lines. Hiding
    // is a visibility decision, not a layout one, so it must apply at every
    // width — unlike the resize machinery, which is deliberately absent
    // here.
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: 'bravo' }).uncheck();

    await page.setViewportSize(NARROW);
    await expect(c.locator('td', { hasText: 'bravo value' })).toBeHidden();
    await expect(c.locator('td', { hasText: 'alpha value' })).toBeVisible();
  });

  test('a hidden column takes no width from the resized ones', async ({ mount, page }) => {
    // A `display: none` cell generates no box, so the browser stops
    // counting it as a column. A <colgroup> that still had an entry for it
    // would apply every later width to the wrong column, and measuring it
    // would store a 0 that reappears as a collapsed column when shown
    // again.
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: 'bravo' }).uncheck();
    await c.locator('summary').click();

    const alpha = c.locator('th', { hasText: 'alpha' });
    const handle = alpha.getByRole('separator');
    const box = (await handle.boundingBox())!;
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width / 2 + 100, box.y + box.height / 2, { steps: 6 });
    await page.mouse.up();

    // Three visible columns ⇒ three <col>s, and no zero-width entry.
    await expect(c.locator('col')).toHaveCount(3);
    const stored = await page.evaluate(() => localStorage.getItem('kanade.table.widths.ct-pick'));
    const widths = JSON.parse(stored!) as Record<string, number>;
    expect(Object.keys(widths).sort()).toEqual(['alpha', 'charlie', 'delta']);
    expect(Math.min(...Object.values(widths))).toBeGreaterThan(0);

    // ...and showing it again gives it a real width, not zero.
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: 'bravo' }).check();
    await expect(c.locator('col')).toHaveCount(4);
    const bravo = c.locator('th', { hasText: 'bravo' });
    expect((await bravo.boundingBox())!.width).toBeGreaterThan(0);
  });
});
