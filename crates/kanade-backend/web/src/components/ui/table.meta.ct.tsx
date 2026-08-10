import { expect, test } from '@playwright/experimental-ct-react';

import {
  TableWithDuplicatePc,
  TableWithLateRows,
  TableWithMetaColumns,
  TableWithPclessRow,
  TableWithPicker,
  TableWithSpanAllPlaceholder,
} from './table.ct.harness';

// #1357: agent_meta values offered as columns on any pc_id-keyed table.
// The endpoints are stubbed — what's under test is the SPA half: that a
// key ticked in the picker becomes a real column, that the values land on
// the right rows, and that it costs nothing until someone ticks one.

const DESKTOP = { width: 1280, height: 720 };

/** Header labels and the cells of each body row, as laid out. */
async function grid(c: {
  locator: (s: string) => {
    allTextContents(): Promise<string[]>;
    nth(i: number): { locator(s: string): { allTextContents(): Promise<string[]> } };
  };
}) {
  const head = (await c.locator('thead th').allTextContents()).map((s) => s.trim());
  const rows = [];
  for (let i = 0; i < 2; i++) {
    rows.push((await c.locator('tbody tr').nth(i).locator('td').allTextContents()).map((s) => s.trim()));
  }
  return { head, rows };
}

test.describe('Table metadata columns', () => {
  test.use({ viewport: DESKTOP });

  let metaRequests = 0;
  let keyRequests = 0;

  test.beforeEach(async ({ page }) => {
    metaRequests = 0;
    keyRequests = 0;
    await page.route('**/api/agents/meta-keys', (route) => {
      keyRequests += 1;
      route.fulfill({ json: ['氏名', '所属'] });
    });
    await page.route('**/api/agents/meta?*', (route) => {
      metaRequests += 1;
      // `pc-b` is absent on purpose: a PC with no attributes is omitted by
      // the endpoint rather than sent as an empty object.
      route.fulfill({
        json: {
          'pc-a': [
            { key: '氏名', value: '山田太郎' },
            { key: '所属', value: '経理' },
          ],
        },
      });
    });
  });

  test('a table without metaColumns never asks the fleet for its keys', async ({ mount }) => {
    // The picker calls the metadata hook unconditionally, so the gate has
    // to be inside it. Most tables in the app have a picker and no
    // metadata — asking every one of them for the fleet's keys is a
    // request for something they can't display.
    const c = await mount(<TableWithPicker />);
    await c.locator('summary').click();
    await expect(c.getByRole('checkbox', { name: 'alpha' })).toBeVisible();
    expect(keyRequests).toBe(0);
  });

  test('offers the fleet-wide keys, none of them ticked', async ({ mount }) => {
    // There is no key this code could sensibly pre-select: they are
    // whatever the fleet's administrator decided to record.
    const c = await mount(<TableWithMetaColumns />);
    await c.locator('summary').click();
    await expect.poll(() => keyRequests).toBeGreaterThan(0);
    for (const key of ['氏名', '所属']) {
      // Offered as something to ADD, not as a ticked column.
      await expect(c.getByRole('button', { name: key })).toBeVisible();
      await expect(c.getByRole('checkbox', { name: key })).toHaveCount(0);
    }
    expect((await grid(c)).head).toEqual(['pc_id', 'os']);
  });

  test('fetches nothing until a key is ticked', async ({ mount, page }) => {
    // A table nobody has added a metadata column to must cost no request.
    const c = await mount(<TableWithMetaColumns />);
    await c.locator('summary').click();
    await expect(c.getByRole('button', { name: '氏名' })).toBeVisible();
    expect(metaRequests).toBe(0);

    await c.getByRole('button', { name: '氏名' }).click();
    await expect.poll(() => metaRequests).toBeGreaterThan(0);
    void page;
  });

  test('a ticked key becomes a column whose values land on the right rows', async ({ mount }) => {
    const c = await mount(<TableWithMetaColumns />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: '氏名' }).click();

    await expect(c.locator('th', { hasText: '氏名' })).toBeVisible();
    await expect.poll(async () => (await grid(c)).rows[0]).toEqual([
      'pc-a',
      'windows',
      '山田太郎',
    ]);
    // `pc-b` has no attributes: an empty cell, not a missing column, and
    // certainly not `pc-a`'s value shifted down a row.
    expect((await grid(c)).rows[1]).toEqual(['pc-b', 'windows', '']);
  });

  test('two keys arrive in the order they were ticked', async ({ mount }) => {
    const c = await mount(<TableWithMetaColumns />);
    await c.locator('summary').click();
    // Ticked 氏名 first even though it sorts AFTER 所属 — the columns must
    // follow the operator's choices, not a collation order they never asked
    // for.
    await c.getByRole('button', { name: '氏名' }).click();
    await c.getByRole('button', { name: '所属' }).click();

    await expect.poll(async () => (await grid(c)).head).toEqual([
      'pc_id',
      'os',
      '氏名',
      '所属',
    ]);
    await expect.poll(async () => (await grid(c)).rows[0]).toEqual([
      'pc-a',
      'windows',
      '山田太郎',
      '経理',
    ]);
  });

  test('a metadata column reorders like any other', async ({ mount }) => {
    // It carries a real id (`meta:<key>`), so the order machinery from
    // #1353 phase 2 treats it as an ordinary column rather than a special
    // case pinned to the end.
    const c = await mount(<TableWithMetaColumns />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: '氏名' }).click();
    await expect(c.locator('th', { hasText: '氏名' })).toBeVisible();

    await c.getByRole('button', { name: 'Move 氏名 left' }).click();
    await expect.poll(async () => (await grid(c)).head).toEqual(['pc_id', '氏名', 'os']);
    // The values move with the heading, as for any other column.
    await expect.poll(async () => (await grid(c)).rows[0]).toEqual([
      'pc-a',
      '山田太郎',
      'windows',
    ]);
  });

  test('unticking a key takes the column away again', async ({ mount, page }) => {
    const c = await mount(<TableWithMetaColumns />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: '氏名' }).click();
    await expect(c.locator('th', { hasText: '氏名' })).toBeVisible();

    // Once added it lives in the main list, where unticking REMOVES it —
    // for a column the operator added themselves, "hide" and "remove" are
    // the same intent, so there is only one control.
    // `.click()`, not `.uncheck()`: the control doesn't become unchecked,
    // it stops existing — removing the key removes the column and with it
    // the row in the picker. `uncheck()` would wait forever for an element
    // that is gone.
    await c.getByRole('checkbox', { name: '氏名' }).click();
    await expect(c.locator('th', { hasText: '氏名' })).toHaveCount(0);
    expect((await grid(c)).head).toEqual(['pc_id', 'os']);
    expect(await page.evaluate(() => localStorage.getItem('kanade.table.meta.ct-meta'))).toBeNull();
  });

  test('a row that appears later pulls its own metadata in', async ({ mount, page }) => {
    // Compliance appends its "ok" rows from a child component that fetches
    // them itself, so the parent never had a list of pc_ids to pass down.
    // The rows announce themselves instead, which is the only way a row
    // that didn't exist at first paint can get its values.
    await page.route('**/api/agents/meta?*', (route) => {
      const pcs = new URL(route.request().url()).searchParams.get('pcs') ?? '';
      const json: Record<string, { key: string; value: string }[]> = {};
      if (pcs.includes('pc-a')) json['pc-a'] = [{ key: '氏名', value: '山田太郎' }];
      if (pcs.includes('pc-late')) json['pc-late'] = [{ key: '氏名', value: '鈴木一郎' }];
      route.fulfill({ json });
    });
    const c = await mount(<TableWithLateRows />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: '氏名' }).click();
    await expect.poll(async () => (await grid(c)).rows[0]).toEqual(['pc-a', 'windows', '山田太郎']);

    await c.getByRole('button', { name: 'expand' }).click();
    await expect.poll(async () =>
      (await c.locator('tbody tr').nth(1).locator('td').allTextContents()).map((s) => s.trim()),
    ).toEqual(['pc-late', 'windows', '鈴木一郎']);
  });

  test('one row leaving does not strip metadata from another with the same pc_id', async ({
    mount,
    page,
  }) => {
    // Compliance lists a PC once per check, so two rows routinely carry the
    // same pc_id. Registration is counted, not a set: if it were a set, the
    // first row to unmount would deregister the PC and the row still on
    // screen would lose its values.
    await page.route('**/api/agents/meta?*', (route) =>
      route.fulfill({ json: { 'pc-a': [{ key: '氏名', value: '山田太郎' }] } }),
    );
    const c = await mount(<TableWithDuplicatePc />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: '氏名' }).click();
    await expect.poll(async () => (await grid(c)).rows[0]).toEqual([
      'pc-a',
      'bitlocker',
      '山田太郎',
    ]);

    await c.getByRole('button', { name: 'drop one' }).click();
    const remaining = c.locator('tbody tr').first().locator('td');
    await expect.poll(async () => (await remaining.allTextContents()).map((s) => s.trim())).toEqual([
      'pc-a',
      'firewall',
      '山田太郎',
    ]);
  });

  test('a spanAll placeholder keeps covering the row as metadata columns appear', async ({
    mount,
  }) => {
    // Pages used to hand-count the `colSpan` on their empty-state rows.
    // That number cannot include metadata columns, which the operator adds
    // at runtime — so the placeholder stopped short and left a gap.
    const c = await mount(<TableWithSpanAllPlaceholder />);
    const cell = c.locator('tbody td');
    expect(await cell.getAttribute('colspan')).toBe('2');

    await c.locator('summary').click();
    await c.getByRole('button', { name: '氏名' }).click();
    await expect(c.locator('th', { hasText: '氏名' })).toBeVisible();
    await expect.poll(() => cell.getAttribute('colspan')).toBe('3');
  });

  test('a data row without a pc_id still takes the column and the reorder', async ({
    mount,
    page,
  }) => {
    // Whether a row takes the metadata columns is decided by its SHAPE, not
    // by whether it has a PC. Keying it off `pcId` meant a row with none
    // skipped the columns and, because its cell count then no longer
    // matched, fell out of the reorder permutation too — rendering its
    // columns in source order under a permuted header.
    await page.route('**/api/agents/meta?*', (route) =>
      route.fulfill({ json: { 'pc-a': [{ key: '氏名', value: '山田太郎' }] } }),
    );
    const c = await mount(<TableWithPclessRow />);
    await c.locator('summary').click();
    await c.getByRole('button', { name: '氏名' }).click();
    await expect(c.locator('th', { hasText: '氏名' })).toBeVisible();

    // The column is there for both rows; only the value differs.
    await expect.poll(async () => (await grid(c)).rows[1]).toEqual(['(none)', 'linux', '']);

    // ...and it follows the permutation with everything else.
    await c.getByRole('button', { name: 'Move 氏名 left' }).click();
    await expect.poll(async () => (await grid(c)).head).toEqual(['pc_id', '氏名', 'os']);
    await expect.poll(async () => (await grid(c)).rows[1]).toEqual(['(none)', '', 'linux']);
    await expect.poll(async () => (await grid(c)).rows[0]).toEqual(['pc-a', '山田太郎', 'windows']);
  });

  test('the choice persists across a remount', async ({ mount, page }) => {
    const first = await mount(<TableWithMetaColumns />);
    await first.locator('summary').click();
    await first.getByRole('button', { name: '氏名' }).click();
    await expect(first.locator('th', { hasText: '氏名' })).toBeVisible();
    expect(
      JSON.parse((await page.evaluate(() => localStorage.getItem('kanade.table.meta.ct-meta')))!),
    ).toEqual(['氏名']);
    await first.unmount();

    const second = await mount(<TableWithMetaColumns />);
    await expect(second.locator('th', { hasText: '氏名' })).toBeVisible();
  });
});
