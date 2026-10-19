import { expect, test } from '@playwright/experimental-ct-react';

import { InventoryRecoveryHarness } from './Inventory.recovery.harness';

test.use({ viewport: { width: 1280, height: 900 } });

for (const view of ['search', 'fleet']) {
  for (const id of ['f:removed', 'meta:removed']) {
    for (const condition of ['filter', 'sort']) {
      test(`${view} recovers from a saved ${condition} on ${id}`, async ({ mount, page }) => {
        const requests: URL[] = [];
        await page.route('**/api/**', async (route) => {
          const url = new URL(route.request().url());
          if (url.pathname === '/api/inventory/jobs') {
            await route.fulfill({ json: [{
              manifest_id: 'test-inventory', description: null, pc_count: 1,
              display: [{ field: 'label', label: 'Label' }], summary: null, explode: null,
            }] });
          } else if (url.pathname.includes('/search-scalars') || url.pathname.includes('/by-job/')) {
            requests.push(url);
            const state = JSON.parse(url.searchParams.get('table') ?? '{}');
            if (state.sort || Object.keys(state.filters ?? {}).length) {
              await route.fulfill({ status: 400, body: 'unknown table column' });
            } else {
              await route.fulfill({ json: {
                rows: [{ pc_id: 'recovered', label: 'restored-label', facts: { label: 'restored-label' }, collected_at: null }],
                total: 1, limit: 100, offset: 0,
              } });
            }
          } else {
            await route.fulfill({ json: [] });
          }
        });
        const state = condition === 'sort'
          ? { sort: { id, dir: 'asc' }, filters: {} }
          : { sort: null, filters: { [id]: { contains: 'old' } } };
        const params = new URLSearchParams(view === 'search'
          ? { manifest: 'test-inventory', field: '__scalars__', offset: '100', table: JSON.stringify(state) }
          : { job: 'test-inventory', fleetTableJob: 'test-inventory', fleetTable: JSON.stringify(state) });
        const path = view === 'search' ? '/inventory/search' : '/inventory';
        const c = await mount(<InventoryRecoveryHarness initial={`${path}?${params}`} />);
        const reset = c.getByRole('button', { name: 'Reset column sorting and filters' });
        await expect(reset).toBeVisible();
        await reset.click();
        await expect(c.locator('tbody').getByText('restored-label', { exact: true })).toBeVisible();
        const request = requests.at(-1)!;
        expect(JSON.parse(request.searchParams.get('table')!)).toEqual({ sort: null, filters: {} });
        expect(Number(request.searchParams.get('offset') ?? 0)).toBe(0);
        await expect.poll(async () => {
          const location = new URL(await c.getByTestId('location').innerText(), 'http://example.test');
          return JSON.parse(location.searchParams.get(view === 'search' ? 'table' : 'fleetTable')!);
        }).toEqual({ sort: null, filters: {} });
        await expect(reset).toHaveCount(0);
      });
    }
  }
}
