import { expect, test } from '@playwright/experimental-ct-react';

import { FleetHarness } from './Inventory.fleet.harness';

// The fleet table reports query state and preserves server-provided rows.

test.use({ viewport: { width: 1280, height: 720 } });

async function pcs(c: { locator(s: string): { allTextContents(): Promise<string[]> } }) {
  return c.locator('tbody tr td:first-child').allTextContents();
}

function head(c: { locator(s: string): { getByRole(r: 'button', o: { name: string; exact: boolean }): { click(): Promise<void> } } }, id: string, name: string) {
  return c.locator(`th[data-col-id="${id}"]`).getByRole('button', { name, exact: true });
}

for (const [id, label] of [['f:ram', 'RAM'], ['collected', 'collected'], ['f:disks', 'Disks']]) {
  test(`${label} requests ascending and descending server order`, async ({ mount }) => {
    const c = await mount(<FleetHarness />);
    const button = head(c, id, label);
    await button.click();
    await expect(c.getByTestId('state')).toContainText(JSON.stringify({ id, dir: 'asc' }));
    expect(await pcs(c)).toEqual(['pc-a', 'pc-b', 'pc-c', 'pc-d']);
    await button.click();
    await expect(c.getByTestId('state')).toContainText(JSON.stringify({ id, dir: 'desc' }));
  });
}

test('filtering a field reports its global checklist selection', async ({ mount, page }) => {
  const c = await mount(<FleetHarness />);
  await c.getByRole('button', { name: 'Filter OS' }).click();
  await page.getByRole('checkbox', { name: /Win10/ }).uncheck();
  await expect(c.getByTestId('state')).toContainText('"values":["Win11"]');
  expect(await pcs(c)).toEqual(['pc-a', 'pc-b', 'pc-c', 'pc-d']);
});

test('row clicks keep the server supplied identity after requesting a sort', async ({ mount }) => {
  const c = await mount(<FleetHarness />);
  await head(c, 'f:ram', 'RAM').click();
  await c.locator('tbody tr').first().click();
  await expect(c.getByTestId('picked')).toHaveText('pc-a');
});
