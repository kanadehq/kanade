import { expect, test } from '@playwright/experimental-ct-react';
import { InstallHistoryHost } from './InstallHistory.ct.harness';

const oldId = '00000000-0000-0000-0000-000000000001';
const newId = '00000000-0000-0000-0000-000000000002';
const old = { id: oldId, created_at: 1793400000, issuer: null, os: 'unix', status: 'unknown', last_stage: null, last_outcome: null };
const recent = { ...old, id: newId, created_at: old.created_at + 60, os: 'linux', issuer: 'installer account with a long readable name', status: 'succeeded', last_stage: 'complete', last_outcome: 'succeeded' };

for (const width of [360, 1280]) {
  test(`readable history, inline selection and wrapping at ${width}px`, async ({ mount, page }, testInfo) => {
    await page.setViewportSize({ width, height: 800 });
    await page.route('**/api/agents/install-attempts', (r) => r.fulfill({ json: [old, recent] }));
    await page.route('**/api/agents/install-attempts/*', (r) => r.fulfill({ json: r.request().url().endsWith(oldId) ? [] : [
      { seq: 2, stage: 'complete', outcome: 'succeeded', exit_code: 0, received_at: recent.created_at },
      { seq: 1, stage: 'connection_check', outcome: 'progress', exit_code: 0, received_at: recent.created_at },
    ] }));
    const component = await mount(<InstallHistoryHost />);
    const rows = component.locator('button[aria-expanded]');
    await expect(rows.first()).toContainText('Installer completed');
    await expect(rows.first()).not.toContainText(newId);
    await expect(rows.first()).toHaveAttribute('aria-expanded', 'true');
    await expect(component.getByText('Agent registration is not confirmed', { exact: false })).toBeVisible();
    await expect(component.locator('ol > li').first()).toContainText('Check connection');
    await page.screenshot({ path: testInfo.outputPath('history.png'), fullPage: true });
    await page.evaluate(() => Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: () => Promise.reject(new Error('denied')) } }));
    await component.getByRole('button', { name: 'Copy attempt ID' }).click();
    await expect(page.getByText('Could not copy. Select the attempt ID above and copy it manually.')).toBeVisible();
    await page.evaluate(() => Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async (value: string) => { localStorage.setItem('copied-attempt', value); } } }));
    await component.getByRole('button', { name: 'Copy attempt ID' }).click();
    expect(await page.evaluate(() => localStorage.getItem('copied-attempt'))).toBe(newId);
    await expect(page.getByText('Attempt ID copied', { exact: true })).toBeVisible();
    await rows.nth(1).click();
    await expect(rows.nth(1)).toHaveAttribute('aria-expanded', 'true');
    await expect(component.getByText('No reports received.', { exact: false })).toBeVisible();
    await expect(component.getByText(oldId, { exact: true })).toBeVisible();
    expect(await rows.nth(1).evaluate((el) => el.getBoundingClientRect().height)).toBeGreaterThanOrEqual(44);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await rows.nth(1).click();
    await expect(rows.nth(1)).toHaveAttribute('aria-expanded', 'false');
    await expect(component.getByRole('button', { name: 'Copy attempt ID' })).toHaveCount(0);
  });
}

test('polling retains selection and stale reports, then clears a removed selection', async ({ mount, page }) => {
  let records = [recent, old];
  let fail = false;
  await page.route('**/api/agents/install-attempts', (r) => r.fulfill(fail ? { status: 500, body: 'temporarily unavailable' } : { json: records }));
  await page.route('**/api/agents/install-attempts/*', (r) => r.fulfill(fail ? { status: 500, body: 'temporarily unavailable' } : { json: [{ seq: 1, stage: 'future_stage', outcome: 'future_outcome', error_kind: 'future_error', exit_code: 42, received_at: old.created_at }] }));
  const component = await mount(<InstallHistoryHost />);
  const rows = component.locator('button[aria-expanded]');
  await rows.nth(1).click();
  await expect(component.getByText('future_stage · future_outcome', { exact: true })).toBeVisible();
  records = [{ ...recent, id: 'new-record', created_at: recent.created_at + 60 }, recent, old];
  await expect(rows).toHaveCount(3, { timeout: 10000 });
  await expect(rows.nth(2)).toHaveAttribute('aria-expanded', 'true');
  await expect(rows.nth(2)).toBeFocused();
  fail = true;
  await expect(component.getByText('Could not refresh installation history')).toBeVisible({ timeout: 10000 });
  await expect(component.getByText('Could not refresh reports')).toBeVisible();
  await expect(rows.nth(2)).toHaveAttribute('aria-expanded', 'true');
  await expect(component.getByText('future_error', { exact: true }).first()).toBeVisible();
  fail = false;
  records = [recent];
  await component.getByRole('button', { name: 'Retry' }).first().click();
  await expect(component.getByRole('status').first()).toContainText('no longer');
  await expect(rows.first()).toHaveAttribute('aria-expanded', 'false');
  await expect(component.getByRole('button', { name: 'Copy attempt ID' })).toHaveCount(0);
});

test('loading, failure, retry and empty history are distinct', async ({ mount, page }) => {
  let respond!: () => void;
  const hold = new Promise<void>((resolve) => { respond = resolve; });
  let fail = true;
  await page.route('**/api/agents/install-attempts', async (r) => {
    await hold;
    await r.fulfill(fail ? { status: 500, body: 'offline' } : { json: [] });
  });
  const component = await mount(<InstallHistoryHost />);
  await expect(component.getByText('Loading installation history…')).toBeVisible();
  respond();
  await expect(component.getByText('Could not refresh installation history')).toBeVisible();
  await expect(component.getByText('No installation attempts yet.')).toHaveCount(0);
  fail = false;
  await component.getByRole('button', { name: 'Retry' }).click();
  await expect(component.getByText('No installation attempts yet.')).toBeVisible();
});


test('Japanese failure stages and technical details remain readable at 375px', async ({ mount, page }, testInfo) => {
  await page.setViewportSize({ width: 375, height: 800 });
  await page.route('**/api/agents/install-attempts', (r) => r.fulfill({ json: [{ ...recent, status: 'failed', last_stage: 'placement', last_outcome: 'failed' }] }));
  await page.route('**/api/agents/install-attempts/*', (r) => r.fulfill({ json: [{ seq: 1, stage: 'placement', outcome: 'failed', error_kind: 'permission_denied', exit_code: 5, received_at: recent.created_at }] }));
  const component = await mount(<InstallHistoryHost language="ja" />);
  await expect(component.locator('ol')).toContainText('エージェントファイルの配置');
  await expect(component.getByText('アクセスが拒否されました。権限を確認してください。')).toBeVisible();
  await component.getByText('技術詳細', { exact: true }).click();
  await expect(component.locator('dd').filter({ hasText: 'permission_denied' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath('history-ja.png'), fullPage: true });
});
