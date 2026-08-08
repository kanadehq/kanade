import { expect, test } from '@playwright/experimental-ct-react';

import { TableWithForcedMove, TableWithoutColIds, TableWithPicker } from './table.ct.harness';

// #1353 phase 2: column order. This changes what React renders in EVERY
// row of every table, so the properties worth asserting are the ones that
// only exist once a browser has laid it out — that the header and the body
// still agree, that rows the permutation must not touch are untouched, and
// that moving a column moves the node rather than rebuilding it.

const DESKTOP = { width: 1280, height: 720 };
/** Below the `lg` card breakpoint, where rows become stacked cards. */
const NARROW = { width: 800, height: 720 };

/** The header labels and the first row's values, as laid out. */
async function layout(c: {
  locator: (s: string) => { allTextContents(): Promise<string[]> };
}): Promise<{ head: string[]; body: string[] }> {
  const head = await c.locator('thead th').allTextContents();
  const body = await c.locator('tbody tr:first-child td').allTextContents();
  return { head: head.map((s) => s.trim()), body: body.map((s) => s.trim()) };
}

test.describe('Table column order', () => {
  test.use({ viewport: DESKTOP });

  test('an untouched table renders its source order', async ({ mount, page }) => {
    const c = await mount(<TableWithPicker />);
    const { head, body } = await layout(c);
    expect(head).toEqual(['alpha', 'bravo', 'charlie', 'delta']);
    expect(body).toEqual(['alpha value', 'bravo value', 'charlie value', 'delta value']);
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.order.ct-pick'))).toBeNull();
  });

  test('moving a column moves the header AND every row cell with it', async ({ mount, page }) => {
    // The failure this exists for: permuting only the header, leaving each
    // row's values under the wrong labels. Nothing about the page's own
    // markup would look wrong — only the pairing would be.
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: 'Move charlie left' }).click();

    const { head, body } = await layout(c);
    expect(head).toEqual(['alpha', 'charlie', 'bravo', 'delta']);
    expect(body).toEqual(['alpha value', 'charlie value', 'bravo value', 'delta value']);
    expect(JSON.parse((await page.evaluate(() => localStorage.getItem('kanade.table.order.ct-pick')))!)).toEqual([
      'alpha',
      'charlie',
      'bravo',
      'delta',
    ]);
  });

  test('a column can be walked to the far end and stops there', async ({ mount }) => {
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    for (let i = 0; i < 3; i++) {
      await c.getByRole('button', { name: 'Move alpha right' }).click();
    }
    const { head, body } = await layout(c);
    expect(head).toEqual(['bravo', 'charlie', 'delta', 'alpha']);
    expect(body).toEqual(['bravo value', 'charlie value', 'delta value', 'alpha value']);
    await expect(c.getByRole('button', { name: 'Move alpha right' })).toBeDisabled();
  });

  test('the store itself refuses to move a column past either end', async ({ mount }) => {
    // The picker disables the arrow at each end, so this is unreachable
    // through the UI — but `useTableColumns` is exported for pages to build
    // their own controls, and a move off the end there must be a no-op, not
    // a wrap or a hole.
    const c = await mount(<TableWithForcedMove />);
    await c.getByRole('button', { name: 'force alpha left' }).click();
    expect((await layout(c)).head).toEqual(['alpha', 'bravo', 'charlie', 'delta']);

    await c.getByRole('button', { name: 'force delta right' }).click();
    const { head, body } = await layout(c);
    expect(head).toEqual(['alpha', 'bravo', 'charlie', 'delta']);
    expect(body).toEqual(['alpha value', 'bravo value', 'charlie value', 'delta value']);
  });

  test('a stored order that predates a new column still renders every column', async ({
    mount,
    page,
  }) => {
    // What happens when a page gains a column after an operator has already
    // arranged the table: the stored order knows nothing about it. It has
    // to be appended, not dropped — a permutation built only from the
    // stored ids is shorter than the row and silently deletes cells.
    await page.evaluate(() =>
      localStorage.setItem('kanade.table.order.ct-pick', JSON.stringify(['delta', 'bravo'])),
    );
    const c = await mount(<TableWithPicker />);
    const { head, body } = await layout(c);
    expect(head).toEqual(['delta', 'bravo', 'alpha', 'charlie']);
    expect(body).toEqual(['delta value', 'bravo value', 'alpha value', 'charlie value']);
  });

  test('a stored order naming a column that no longer exists is ignored', async ({
    mount,
    page,
  }) => {
    await page.evaluate(() =>
      localStorage.setItem(
        'kanade.table.order.ct-pick',
        JSON.stringify(['gone', 'charlie', 'alpha', 'bravo', 'delta']),
      ),
    );
    const c = await mount(<TableWithPicker />);
    const { head, body } = await layout(c);
    expect(head).toEqual(['charlie', 'alpha', 'bravo', 'delta']);
    expect(body).toEqual(['charlie value', 'alpha value', 'bravo value', 'delta value']);
  });

  test('a release that inserts a column drops positional prefs instead of misapplying them', async ({
    mount,
    page,
  }) => {
    // Headers with no `colId` are identified by position. A bare index
    // would shift when a column is inserted and quietly hand every stored
    // preference to its neighbour; folding the column count into the id
    // makes the stored entries stop matching instead.
    const first = await mount(<TableWithoutColIds />);
    await first.locator('summary').click();
    await first.getByRole('button', { name: 'Move delta left' }).click();
    expect((await layout(first)).head).toEqual(['alpha', 'bravo', 'delta', 'charlie']);
    const stored = await page.evaluate(() => localStorage.getItem('kanade.table.order.ct-noid'));
    expect(JSON.parse(stored!)).toEqual(['0/4', '1/4', '3/4', '2/4']);
    await first.unmount();

    // Same table, one more column — the stored order is now about a shape
    // that no longer exists, so it is ignored rather than applied.
    const second = await mount(<TableWithoutColIds inserted />);
    expect((await layout(second)).head).toEqual([
      'alpha',
      'inserted',
      'bravo',
      'charlie',
      'delta',
    ]);
  });

  test('an empty-state colSpan row is never permuted', async ({ mount }) => {
    // A row whose cell count doesn't match the header can't be permuted
    // meaningfully — its cells don't correspond to columns. `bravo` is
    // moved to the front on purpose: that permutation starts `[1, 0, …]`,
    // so an unguarded row would swap its spacer and its message and render
    // the text first.
    const c = await mount(<TableWithPicker empty />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: 'Move bravo left' }).click();

    const cells = c.locator('tbody tr:first-child td');
    expect(await cells.count()).toBe(2);
    expect((await cells.nth(0).textContent())?.trim()).toBe('');
    expect((await cells.nth(1).textContent())?.trim()).toBe('nothing here');
    const head = await c.locator('thead th').allTextContents();
    expect(head.map((s) => s.trim())).toEqual(['bravo', 'alpha', 'charlie', 'delta']);
  });

  test('the order survives a remount', async ({ mount }) => {
    const first = await mount(<TableWithPicker />);
    await first.locator('summary').click();
    await first.getByRole('button', { name: 'Move delta left' }).click();
    await first.unmount();

    const second = await mount(<TableWithPicker />);
    const { head, body } = await layout(second);
    expect(head).toEqual(['alpha', 'bravo', 'delta', 'charlie']);
    expect(body).toEqual(['alpha value', 'bravo value', 'delta value', 'charlie value']);
  });

  test('state and focus inside a cell survive a reorder', async ({ mount }) => {
    // This is why the permutation goes through React's children rather than
    // the DOM: `Children.toArray` keys the cells, so a reorder is a *move*
    // and everything living inside a cell — a half-typed filter, an open
    // menu, the caret — comes with it. Rebuilding the nodes would silently
    // discard all of it, and nothing about the layout would look wrong.
    const c = await mount(<TableWithPicker withInput />);
    const input = c.locator('tbody input');
    await input.click();
    await input.fill('half typed');

    await c.locator('summary').click();
    await c.getByRole('button', { name: 'Move charlie left' }).click();

    // The VALUE is the assertion. Focus is not: reordering is driven from
    // the picker, and clicking in there necessarily blurs the input — so a
    // focus check would be testing the test, not the permutation.
    await expect(c.locator('tbody input')).toHaveValue('half typed');
  });

  test('reset puts the columns back in source order', async ({ mount, page }) => {
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: 'Move charlie left' }).click();
    await c.getByRole('button', { name: 'Reset column order' }).click();

    const { head, body } = await layout(c);
    expect(head).toEqual(['alpha', 'bravo', 'charlie', 'delta']);
    expect(body).toEqual(['alpha value', 'bravo value', 'charlie value', 'delta value']);
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.order.ct-pick'))).toBeNull();
    await expect(c.getByRole('button', { name: 'Reset column order' })).toHaveCount(0);
  });

  test('card mode stacks the label:value lines in the chosen order', async ({ mount, page }) => {
    // Below the breakpoint each row is a stack of label:value lines. Order
    // is a content decision, not a layout one, so it has to apply here too
    // — unlike the resize machinery, which is deliberately absent.
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: 'Move delta left' }).click();

    await page.setViewportSize(NARROW);
    const cells = await c.locator('tbody tr:first-child td').allTextContents();
    expect(cells.map((s) => s.trim())).toEqual([
      'alpha value',
      'bravo value',
      'delta value',
      'charlie value',
    ]);
  });

  test('order and hiding compose', async ({ mount }) => {
    // Hiding is positional (a stylesheet keyed on `nth-child`) and order
    // changes those positions, so the two have to be resolved against the
    // same display order or the wrong column disappears.
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: 'Move delta left' }).click();
    await c.getByRole('checkbox', { name: 'charlie' }).uncheck();

    await expect(c.locator('th', { hasText: 'charlie' })).toBeHidden();
    await expect(c.locator('th', { hasText: 'delta' })).toBeVisible();
    const visible = await c
      .locator('thead th:visible')
      .allTextContents();
    expect(visible.map((s) => s.trim())).toEqual(['alpha', 'bravo', 'delta']);
  });
});
