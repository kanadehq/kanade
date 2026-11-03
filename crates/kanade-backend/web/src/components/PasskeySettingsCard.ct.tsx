import { test, expect } from '@playwright/experimental-ct-react';
import { PasskeySettingsHost } from './PasskeySettingsCard.ct.harness';

test('saves only edited fields, refreshes effective state and public probe, and persists after remount', async ({ mount, page }) => {
  await page.evaluate(() => localStorage.setItem('i18nextLng', 'en'));
  let stored = { enabled: false, rp_id: '', rp_origins: [] as string[], rp_name: null };
  const patches: unknown[] = [];
  await page.route('**/api/**', async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/api/server-settings') {
      if (route.request().method() === 'PUT') {
        const body = route.request().postDataJSON();
        patches.push(body);
        stored = { ...stored, ...body.passkeys };
      }
      return route.fulfill({ json: { passkeys: stored } });
    }
    if (path.endsWith('/status')) return route.fulfill({ json: { state: stored.enabled ? 'enabled' : 'disabled' } });
    return route.fulfill({ json: { enabled: stored.enabled } });
  });
  let component = await mount(<PasskeySettingsHost />);
  await expect(component.getByRole('status')).toHaveText('Effective state: disabled');
  await component.getByLabel('Enable passkeys').check();
  await component.getByLabel('RP ID (bare lowercase host name)').fill('example.com');
  await component.getByLabel('Allowed origins (one per line)').fill('https://example.com\nhttps://login.example.com');
  await component.getByRole('button', { name: 'Save passkey settings' }).click();
  await expect(component.getByRole('status')).toHaveText('Effective state: enabled');
  await expect(component.getByTestId('public-probe')).toHaveText('true');
  expect(patches[0]).toEqual({ passkeys: { enabled: true, rp_id: 'example.com', rp_origins: ['https://example.com', 'https://login.example.com'] } });
  await component.unmount();
  component = await mount(<PasskeySettingsHost />);
  await expect(component.getByLabel('RP ID (bare lowercase host name)')).toHaveValue('example.com');
  await component.getByLabel('Enable passkeys').uncheck();
  await component.getByRole('button', { name: 'Save passkey settings' }).click();
  await expect(component.getByRole('status')).toHaveText('Effective state: disabled');
  await expect(component.getByTestId('public-probe')).toHaveText('false');
  expect(patches[1]).toEqual({ passkeys: { enabled: false } });
});

test('viewer cannot edit and failed reads are visible', async ({ mount, page }) => {
  await page.evaluate(() => localStorage.setItem('i18nextLng', 'en'));
  await page.route('**/api/**', (route) => route.fulfill({ status: 503, body: 'unavailable' }));
  const component = await mount(<PasskeySettingsHost canOperate={false} />);
  await expect(component.getByLabel('Enable passkeys')).toBeDisabled();
  await expect(component.getByRole('button')).toBeDisabled();
  await expect(component.getByRole('status')).toContainText('unavailable');
});

test('invalid update keeps the saved state and exposes the validation error', async ({ mount, page }) => {
  await page.evaluate(() => localStorage.setItem('i18nextLng', 'en'));
  await page.route('**/api/**', (route) => {
    if (route.request().method() === 'PUT') return route.fulfill({ status: 422, body: 'rp_origins must be https' });
    const path = new URL(route.request().url()).pathname;
    return route.fulfill({ json: path.endsWith('/status') ? { state: 'disabled' } : {} });
  });
  const component = await mount(<PasskeySettingsHost />);
  await component.getByLabel('Allowed origins (one per line)').fill('http://example.com');
  await component.getByRole('button').click();
  await expect(component.getByRole('alert')).toContainText('rp_origins must be https');
  await expect(component.getByRole('status')).toHaveText('Effective state: disabled');
});
