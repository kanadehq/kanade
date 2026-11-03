import { expect, test } from '@playwright/experimental-ct-react';

import { AgentsTableHarness } from './Agents.table.harness';

// The Rust producer integration test serves the real Agents API and SQLite
// projection. There are no intercepted requests or handwritten credential rows.
test('broker-produced WSS identity reaches Agents and expires', async ({ mount, page }) => {
  test.skip(!process.env.KANADE_IDENTITY_API, 'requires the producer/consumer verification runner');
  let c = await mount(<AgentsTableHarness />);
  let row = c.locator('tbody tr').filter({ hasText: 'PC001' });
  const named = row.getByText('agent', { exact: true });
  await expect(named).toBeVisible();
  await expect(named).toHaveClass(/bg-success/);
  const health = await page.request.get(`${process.env.KANADE_IDENTITY_API}/test/health`);
  expect(health.ok()).toBe(true);
  expect((await health.json()).open_total).toBe(0);
  const expired = await page.request.post(`${process.env.KANADE_IDENTITY_API}/test/expire`);
  expect(expired.status()).toBe(204);
  await c.unmount();
  c = await mount(<AgentsTableHarness />);
  row = c.locator('tbody tr').filter({ hasText: 'PC001' });
  const unproven = row.getByText('unproven', { exact: true });
  await expect(unproven).toBeVisible();
  await expect(unproven).not.toHaveClass(/bg-success/);
  await expect(row.getByText('agent', { exact: true })).toHaveCount(0);
  for (const forbidden of ['fixture-password', 'broker-claim-canary', '127.0.0.1']) {
    expect(await c.textContent()).not.toContain(forbidden);
  }
});
