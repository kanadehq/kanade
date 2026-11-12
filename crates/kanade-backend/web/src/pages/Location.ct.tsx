import { expect, test } from '@playwright/experimental-ct-react';

import App from '@/App';

test('location-only operator can navigate, view and request without commons APIs', async ({ mount, page }) => {
  await page.evaluate(() => {
    localStorage.setItem('kanade_token', 'test-token');
    localStorage.setItem('kanade_lang', 'en');
    window.history.replaceState({}, '', '/location');
  });
  const calls: string[] = [];
  await page.route('**/api/**', async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    calls.push(`${request.method()} ${path}`);
    const json = path === '/api/auth/me'
      ? { username: 'test', role: 'operator', allowed_features: ['location'] }
      : path === '/api/version'
        ? { version: 'test' }
        : path.startsWith('/api/agents/PC-1/location')
          ? { enabled: true, last_known: null, latest_request: null }
          : null;
    await route.fulfill({ status: json ? 200 : 403, json });
  });
  await mount(<App />);
  await expect(page.getByRole('heading', { name: 'Device location', exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Device location' })).toBeVisible();
  await page.getByLabel('pc_id', { exact: true }).fill('PC-1');
  await page.getByRole('button', { name: 'Show device' }).click();
  await expect(page.getByText('No location reading has been recorded for this device.')).toBeVisible();
  await page.getByRole('button', { name: 'Request location', exact: true }).click();
  await expect.poll(() => calls.includes('POST /api/agents/PC-1/location/request')).toBe(true);
  expect(calls.filter((call) => !call.includes('/location') && !call.endsWith('/api/auth/me') && !call.endsWith('/api/version'))).toEqual([]);
});
