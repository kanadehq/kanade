import { expect, test } from '@playwright/experimental-ct-react';

import type { ProcessesResponse, ProcessRow } from '@/lib/types';

import { AgentProcessSection } from './AgentProcessSection';

const process = (pid: number, name: string, cpu: number, bytes: number, rate: number | null): ProcessRow => ({
  pid, name, cpu_pct: cpu, rss_bytes: bytes,
  disk_read_bytes_per_sec: rate, disk_written_bytes_per_sec: rate,
});
const initial = [
  process(100, 'alpha', 100, 1.2 * 1024 ** 3, 1024 ** 3),
  process(2, 'Bravo', 2, 980 * 1024 ** 2, 980 * 1024 ** 2),
  process(30, 'aardvark', 30, 10, null),
];

// The CT runner does not re-export the browser types; only these API
// mocking and clock methods are needed by the shared polling fixture.
type ApiPage = {
  route(pattern: string, handler: (route: {
    request(): { url(): string };
    fulfill(response: { status?: number; body?: string; json?: unknown }): Promise<void>;
  }) => Promise<void>): Promise<unknown>;
  clock: { runFor(ms: number): Promise<void> };
};

async function mockProcesses(page: ApiPage) {
  let data: ProcessesResponse = { pc_id: 'test', latest_at: '2026-10-01T00:00:00Z', processes: initial };
  let polls = 0;
  let fail = false;
  const timelineRequests: URL[] = [];
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.endsWith('/processes')) {
      polls++;
      await route.fulfill(fail ? { status: 500, body: 'unavailable' } : { json: data });
    } else if (url.pathname.endsWith('/timeline')) {
      timelineRequests.push(url);
      await route.fulfill({ json: { names: [], points: [] } });
    } else {
      await route.fulfill({ json: {} });
    }
  });
  return {
    set(rows: ProcessRow[]) { data = { ...data, latest_at: '2026-10-01T00:01:00Z', processes: rows }; },
    fail() { fail = true; },
    async poll() {
      const previous = polls;
      await page.clock.runFor(5100);
      await expect.poll(() => polls).toBeGreaterThan(previous);
    },
    timelineRequests,
  };
}

test.describe('Process snapshot sorting', () => {
  test.use({ viewport: { width: 1280, height: 1000 } });

  test('uses raw numeric values, case-insensitive names, picker and resize controls', async ({ mount, page }) => {
    const api = await mockProcesses(page);
    const c = await mount(<AgentProcessSection pcId="test" />);
    const names = c.locator('tbody code');
    await expect(names).toHaveText(['alpha', 'Bravo', 'aardvark']);
    for (const [column, expected] of [
      ['pid', ['Bravo', 'aardvark', 'alpha']],
      ['cpu %', ['Bravo', 'aardvark', 'alpha']],
      ['rss', ['aardvark', 'Bravo', 'alpha']],
      ['name', ['aardvark', 'alpha', 'Bravo']],
      ['disk read/s', ['Bravo', 'alpha', 'aardvark']],
      ['disk written/s', ['Bravo', 'alpha', 'aardvark']],
    ] as const) {
      const button = c.getByRole('button', { name: column, exact: true });
      await button.click();
      await expect(names).toHaveText([...expected]);
      if (column.startsWith('disk')) {
        await button.click();
        await expect(names).toHaveText(['alpha', 'Bravo', 'aardvark']);
      }
    }
    const pid = c.locator('th').filter({ has: page.getByRole('button', { name: 'pid', exact: true }) });
    const handle = pid.getByRole('separator');
    await handle.focus();
    await handle.press('ArrowRight');
    await expect.poll(() => page.evaluate(() => localStorage.getItem('kanade.table.widths.agent.processes'))).toBeTruthy();
    await c.locator('summary').click();
    await c.getByRole('checkbox', { name: /^rss\b/ }).uncheck();
    await expect(c.getByRole('button', { name: 'rss', exact: true })).toBeHidden();
    await c.getByRole('checkbox', { name: /^rss\b/ }).check();
    await c.locator('summary').click();
    await c.locator('select').filter({ has: page.locator('option[value="10"]') }).selectOption('10');
    await expect.poll(() => api.timelineRequests.at(-1)?.searchParams.get('top')).toBe('10');
    await expect(names).toHaveCount(3);
    await expect(c.getByText(/this page only/i)).toHaveCount(0);
  });
});

for (const width of [1280, 800]) {
  test.describe(`Polling process snapshot at ${width}px`, () => {
    test.use({ viewport: { width, height: 1000 } });

    test('holds rows, candidates and timestamp during filtering, then applies the latest poll', async ({ mount, page }) => {
      await page.clock.install();
      const api = await mockProcesses(page);
      const c = await mount(<AgentProcessSection pcId="test" />);
      const names = c.locator('tbody code');
      await expect(names).toHaveCount(3);
      if (width === 1280) {
        await c.getByRole('button', { name: 'name', exact: true }).click();
        await c.getByRole('button', { name: 'Filter name', exact: true }).click();
      } else {
        await expect(c.locator('thead')).toBeHidden();
        await c.getByRole('button', { name: 'Sort / filter', exact: true }).click();
        await page.getByRole('dialog').locator('select').selectOption({ label: 'name' });
        await page.getByRole('dialog').getByRole('button', { name: 'Ascending', exact: true }).click();
      }
      const input = page.getByRole('searchbox', { name: 'name contains', exact: true });
      await input.fill('a');
      const sampled = await c.getByText(/^Sampled at/).textContent();
      const candidates = await page.getByRole('dialog').locator('label').allTextContents();
      await names.filter({ hasText: /^alpha$/ }).evaluate((node) => node.setAttribute('data-original-row', 'true'));
      api.set([process(100, 'alpha', 1, 1, 1), process(7, 'delta', 7, 7, 7)]);
      await api.poll();
      await expect(input).toBeFocused();
      await expect(input).toHaveValue('a');
      await expect(names).toHaveText(['aardvark', 'alpha', 'Bravo']);
      await expect(c.getByText(/^Sampled at/)).toHaveText(sampled!);
      expect(await page.getByRole('dialog').locator('label').allTextContents()).toEqual(candidates);
      await expect(page.getByRole('dialog').getByText('delta', { exact: true })).toHaveCount(0);
      await input.fill('al');
      await expect(names).toHaveText(['alpha']);
      await input.fill('a');
      // Switching panels must not release the held snapshot between columns.
      if (width === 1280) await c.getByRole('button', { name: 'Filter cpu %', exact: true }).click();
      else await page.getByRole('dialog').locator('select').selectOption({ label: 'cpu %' });
      await expect(names).toHaveText(['aardvark', 'alpha', 'Bravo']);
      await page.keyboard.press('Escape');
      await expect(names).toHaveText(['alpha', 'delta']);
      await expect(names.first()).toHaveAttribute('data-original-row', 'true');
      await expect(c.getByTestId('filter-chip')).toContainText('name');
      api.set([]);
      await api.poll();
      await expect(names).toHaveCount(0);
      await expect(c.locator('table')).toBeAttached();
      await expect(c.getByTestId('filter-chip')).toContainText('name');
      api.set([process(7, 'delta', 7, 7, 7), process(100, 'alpha', 1, 1, 1), process(9, 'xyz', 9, 9, 9)]);
      await api.poll();
      await expect(names).toHaveText(['alpha', 'delta']);
      api.fail();
      await api.poll();
      await expect(c.getByText("Couldn't load process data", { exact: true })).toBeVisible();
      await expect(names).toHaveText(['alpha', 'delta']);
    });
  });
}
