import { expect, test } from '@playwright/experimental-ct-react';

import { FleetHarness } from './Inventory.fleet.harness';

// The fleet table opts into per-column sort/filter. These cover what only a
// real table shows: sort keys behind formatted cells, blanks last in both
// directions, filtering, and that a click still resolves to the right PC
// after the rows were re-ordered.

test.use({ viewport: { width: 1280, height: 720 } });

async function pcs(c: { locator(s: string): { allTextContents(): Promise<string[]> } }) {
  return c.locator('tbody tr td:first-child').allTextContents();
}

function head(c: { locator(s: string): { getByRole(r: 'button', o: { name: string; exact: boolean }): { click(): Promise<void> } } }, id: string, name: string) {
  return c.locator(`th[data-col-id="${id}"]`).getByRole('button', { name, exact: true });
}

test('bytes column sorts numerically, blanks last both ways', async ({ mount }) => {
  const c = await mount(<FleetHarness />);
  const b = head(c, 'f:ram', 'RAM');
  await b.click();
  expect(await pcs(c)).toEqual(['pc-b', 'pc-a', 'pc-c', 'pc-d']);
  await b.click();
  expect(await pcs(c)).toEqual(['pc-c', 'pc-a', 'pc-b', 'pc-d']);
});

test('collected sorts chronologically', async ({ mount }) => {
  const c = await mount(<FleetHarness />);
  await head(c, 'collected', 'collected').click();
  const th = c.locator('th[data-col-id="collected"]');
  await expect(th).toHaveAttribute('aria-sort', 'ascending');
  expect(await pcs(c)).toEqual(['pc-b', 'pc-a', 'pc-c', 'pc-d']);
});

test('nested table column sorts by row count', async ({ mount }) => {
  const c = await mount(<FleetHarness />);
  await head(c, 'f:disks', 'Disks').click();
  expect(await pcs(c)).toEqual(['pc-b', 'pc-a', 'pc-c', 'pc-d']);
});

test('filtering a field column drops rows', async ({ mount, page }) => {
  const c = await mount(<FleetHarness />);
  await c.getByRole('button', { name: 'Filter OS' }).click();
  await page.getByRole('checkbox', { name: 'Win10' }).uncheck();
  expect(await pcs(c)).toEqual(['pc-a', 'pc-c']);
});

test('clicking a row after sorting picks that PC', async ({ mount }) => {
  const c = await mount(<FleetHarness />);
  await head(c, 'f:ram', 'RAM').click();
  await c.locator('tbody tr').first().click();
  await expect(c.getByTestId('picked')).toHaveText('pc-b');
});
