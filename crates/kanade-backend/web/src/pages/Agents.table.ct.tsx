import { expect, test } from '@playwright/experimental-ct-react';
import type { Page } from 'playwright/test';

import { AgentsTableHarness } from './Agents.table.harness';

// The Agents table sorts and filters the WHOLE fleet on the server: the
// mocked endpoint below holds 120 agents (three pages) and answers like the
// real one — filters first, then order, then limit/offset — so these tests
// can tell a fleet-wide result from a page-local re-sort.

test.use({ viewport: { width: 1280, height: 900 } });

type Agent = { agent_version: string; last_logon_user: string; pc_id: string; os_family: string; agent_cpu_pct: number | null; last_heartbeat: string | null };
const FLEET: Agent[] = Array.from({ length: 120 }, (_, i) => ({
  agent_version: i % 2 === 0 ? '1.0' : '2.0',
  last_logon_user: i % 2 === 0 ? 'alice' : 'bob',
  pc_id: `pc-${String(i).padStart(3, '0')}`,
  os_family: i % 3 === 0 ? 'linux' : 'windows',
  agent_cpu_pct: i % 10 === 9 ? null : i,
  last_heartbeat: null,
}));

interface Recorded { url: URL }

async function mockFleet(page: Page) {
  const agentRequests: Recorded[] = [];
  const distinctRequests: Recorded[] = [];
  await page.route('**/api/**', async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname !== '/api/agents') {
      await route.fulfill({ json: [] });
      return;
    }
    const state = JSON.parse(url.searchParams.get('table') ?? '{}') as {
      sort?: { id: string; dir: 'asc' | 'desc' } | null;
      filters?: Record<string, { contains?: string; values?: (string | null)[] }>;
    };
    if (Object.keys(state.filters ?? {}).includes('meta:removed') || state.sort?.id === 'meta:removed') {
      await route.fulfill({ status: 400, body: 'unknown metadata column' });
      return;
    }
    const distinct = url.searchParams.get('distinct');
    const textOf = (a: Agent, id: string) => (id === 'os' ? a.os_family : id === 'pcId' ? a.pc_id : id === 'agent' ? a.agent_version : id === 'lastLogon' ? a.last_logon_user : null);
    const matches = (a: Agent, skip?: string | null) => {
      for (const [param, id] of Object.entries({ q: 'pcId', user: 'lastLogon', version: 'agent' })) {
        const pattern = url.searchParams.get(param);
        if (id !== skip && pattern && !new RegExp(pattern).test(textOf(a, id) ?? '')) return false;
      }
      return Object.entries(state.filters ?? {}).every(([id, f]) => {
        if (id === skip) return true;
        const text = textOf(a, id);
        if (f.contains && !(text ?? '').toLowerCase().includes(f.contains.toLowerCase())) return false;
        return !f.values || f.values.includes(text);
      });
    };
    if (distinct) {
      distinctRequests.push({ url });
      const counts = new Map<string, number>();
      for (const a of FLEET.filter((x) => matches(x, distinct))) {
        const v = textOf(a, distinct) ?? '';
        counts.set(v, (counts.get(v) ?? 0) + 1);
      }
      await route.fulfill({ json: { values: [...counts].map(([value, count]) => ({ value, count })), overflow: false } });
      return;
    }
    agentRequests.push({ url });
    let rows = FLEET.filter((a) => matches(a));
    const sort = state.sort;
    if (sort?.id === 'cpu') {
      const dir = sort.dir === 'asc' ? 1 : -1;
      rows = [...rows].sort((a, b) => {
        if (a.agent_cpu_pct === null || b.agent_cpu_pct === null) {
          return (a.agent_cpu_pct === null ? 1 : 0) - (b.agent_cpu_pct === null ? 1 : 0);
        }
        return dir * (a.agent_cpu_pct - b.agent_cpu_pct) || a.pc_id.localeCompare(b.pc_id);
      });
    }
    const limit = url.searchParams.get('limit');
    const offset = Number(url.searchParams.get('offset') ?? 0);
    await route.fulfill({
      json: limit ? rows.slice(offset, offset + Number(limit)) : rows,
      headers: {
        'X-Total-Count': String(rows.length),
        'X-Online-Count': '0',
        'X-Offline-Count': String(rows.length),
      },
    });
  });
  return { agentRequests, distinctRequests };
}

const firstCells = (c: { locator(s: string): { allTextContents(): Promise<string[]> } }) =>
  c.locator('tbody tr td:nth-child(2) code').allTextContents();

test('header sort is a server sort over the whole fleet and has no page-only notice', async ({ mount, page }) => {
  const { agentRequests } = await mockFleet(page);
  const c = await mount(<AgentsTableHarness />);
  await expect(c.locator('tbody tr')).toHaveCount(50);
  const cpu = c.locator('th[data-col-id="cpu"]').getByRole('button', { name: 'agent CPU', exact: true });
  await cpu.click();
  await cpu.click();
  // Descending across all 120 agents: 118 is the highest non-null CPU, on
  // what is page 3 in the default order.
  await expect.poll(async () => (await firstCells(c))[0]).toBe('pc-118');
  const last = JSON.parse(agentRequests.at(-1)!.url.searchParams.get('table')!);
  expect(last.sort).toEqual({ id: 'cpu', dir: 'desc' });
  await expect(c.getByText(/this page only/i)).toHaveCount(0);
  await expect(c.getByTestId('location')).toContainText('sort=cpu&dir=desc');
});

test('checklist candidates come from the whole fleet and the filter narrows total and pages', async ({ mount, page }) => {
  const { agentRequests, distinctRequests } = await mockFleet(page);
  const c = await mount(<AgentsTableHarness />);
  await expect(c.locator('tbody tr')).toHaveCount(50);
  await c.getByRole('button', { name: 'Filter os' }).click();
  await page.getByRole('checkbox', { name: /linux/ }).waitFor();
  expect(JSON.parse(distinctRequests.at(-1)!.url.searchParams.get('table')!).filters).toEqual({});
  expect(distinctRequests.at(-1)!.url.searchParams.get('distinct')).toBe('os');
  await page.getByRole('checkbox', { name: /windows/ }).uncheck();
  await expect.poll(() => agentRequests.at(-1)!.url.searchParams.get('table')).toContain('"values":["linux"]');
  // 40 linux agents: one page, no pager.
  await expect(c.locator('tbody tr')).toHaveCount(40);
  expect(agentRequests.at(-1)!.url.searchParams.get('offset')).toBe('0');
});

test('legacy sort and regex URLs seed the same state and request', async ({ mount, page }) => {
  const { agentRequests } = await mockFleet(page);
  await mount(<AgentsTableHarness initial="/agents?sort=last_heartbeat&dir=desc&q=pc-0&cf=%7B%22filters%22%3A%7B%22os%22%3A%7B%22contains%22%3A%22lin%22%7D%7D%7D" />);
  await expect.poll(() => agentRequests.length).toBeGreaterThan(0);
  const url = agentRequests[0].url;
  expect(url.searchParams.get('q')).toBe('pc-0');
  expect(JSON.parse(url.searchParams.get('table')!)).toEqual({
    sort: { id: 'lastHeartbeat', dir: 'desc' },
    filters: { os: { contains: 'lin' } },
  });
});

for (const [param, column, initial, replacement, expected] of [
  ['q', 'pcId', '^pc-00', 'pc-11', 10],
  ['user', 'lastLogon', '^alice$', 'bob', 60],
  ['version', 'agent', '^1.0$', '2.0', 60],
] as const) {
  test(`legacy ${param} is replaced and cleared through its column control`, async ({ mount, page }) => {
    const { agentRequests } = await mockFleet(page);
    const c = await mount(<AgentsTableHarness initial={`/agents?${param}=${encodeURIComponent(initial)}`} />);
    await expect(c.locator('tbody tr')).toHaveCount(param === 'q' ? 10 : 50);
    await expect(c.locator('#agents-q, #agents-user, #agents-version')).toHaveCount(0);
    await c.locator(`th[data-col-id="${column}"] button`).last().click();
    const input = page.getByRole('dialog').getByRole('searchbox', { name: /contains/ });
    await expect(input).toHaveValue(initial);
    await input.fill(replacement);
    await expect.poll(() => agentRequests.at(-1)?.url.searchParams.has(param)).toBe(false);
    await expect.poll(() => JSON.parse(agentRequests.at(-1)!.url.searchParams.get('table')!).filters[column])
      .toEqual({ contains: replacement });
    await expect.poll(() => agentRequests.at(-1)!.url.searchParams.get('offset')).toBe('0');
    await expect(c.locator('tbody tr')).toHaveCount(Math.min(expected, 50));
    await input.fill('');
    await expect.poll(() => JSON.parse(agentRequests.at(-1)!.url.searchParams.get('table')!).filters)
      .toEqual({});
    await expect(c.getByTestId('location')).not.toContainText(`${param}=`);
    await expect(c.locator('tbody tr')).toHaveCount(50);
  });
}

test('a checklist replaces a legacy version regex and candidates include other versions', async ({ mount, page }) => {
  const { agentRequests } = await mockFleet(page);
  const c = await mount(<AgentsTableHarness initial="/agents?version=1.0" />);
  await expect(c.locator('tbody tr')).toHaveCount(50);
  await c.getByRole('button', { name: 'Filter agent', exact: true }).click();
  await page.getByRole('checkbox', { name: /2.0/ }).waitFor();
  await page.getByRole('checkbox', { name: /1.0/ }).uncheck();
  await expect.poll(() => agentRequests.at(-1)!.url.searchParams.has('version')).toBe(false);
  await expect.poll(() => JSON.parse(agentRequests.at(-1)!.url.searchParams.get('table')!).filters.agent.values)
    .toEqual(['2.0']);
  await expect.poll(async () => (await firstCells(c))[0]).toBe('pc-001');
});

test('CSV export sends the active column filters without a page limit', async ({ mount, page }) => {
  const { agentRequests } = await mockFleet(page);
  const c = await mount(<AgentsTableHarness initial="/agents?cf=%7B%22filters%22%3A%7B%22os%22%3A%7B%22values%22%3A%5B%22linux%22%5D%7D%7D%7D" />);
  await expect(c.locator('tbody tr')).toHaveCount(40);
  const before = agentRequests.length;
  await c.getByRole('button', { name: 'Export CSV' }).click();
  await expect.poll(() => agentRequests.length).toBeGreaterThan(before);
  const exportUrl = agentRequests.at(-1)!.url;
  expect(exportUrl.searchParams.has('limit')).toBe(false);
  expect(JSON.parse(exportUrl.searchParams.get('table')!).filters).toEqual({ os: { values: ['linux'] } });
});

test('a stale saved column condition can be reset', async ({ mount, page }) => {
  await mockFleet(page);
  const c = await mount(<AgentsTableHarness initial="/agents?cf=%7B%22filters%22%3A%7B%22meta%3Aremoved%22%3A%7B%22contains%22%3A%22x%22%7D%7D%7D" />);
  const reset = c.getByRole('button', { name: 'Reset column sorting and filters' });
  await reset.click();
  await expect(c.locator('tbody tr')).toHaveCount(50);
  await expect(c.getByTestId('location')).not.toContainText('cf=');
});

test.describe('mobile card layout', () => {
  test.use({ viewport: { width: 390, height: 800 } });

  test('rows stay cards and the card-mode sort control drives the server sort', async ({ mount, page }) => {
    const { agentRequests } = await mockFleet(page);
    const c = await mount(<AgentsTableHarness />);
    await expect(c.locator('tbody tr')).toHaveCount(50);
    await expect(c.locator('thead')).toBeHidden();
    await c.getByRole('button', { name: 'Sort / filter' }).click();
    const panel = page.getByRole('dialog');
    await panel.getByRole('combobox').selectOption({ label: 'agent CPU' });
    await panel.getByRole('button', { name: 'Descending' }).click();
    await expect.poll(() => JSON.parse(agentRequests.at(-1)!.url.searchParams.get('table')!).sort)
      .toEqual({ id: 'cpu', dir: 'desc' });
    await expect.poll(async () => (await c.locator('tbody tr').first().innerText()).includes('pc-118')).toBe(true);
  });
});
