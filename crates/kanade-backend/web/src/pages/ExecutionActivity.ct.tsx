import { expect, test } from '@playwright/experimental-ct-react';

import { activityUrl } from '@/lib/activityUrl';

import { ExecutionActivityHost } from './ExecutionActivity.ct.harness';

test.beforeEach(async ({ page }) => {
  await page.evaluate(() => {
    localStorage.setItem('kanade_token', 'test-token');
    localStorage.setItem('kanade_lang', 'en');
  });
  await page.route('**/api/**', async (route) => {
    const path = new URL(route.request().url()).pathname;
    const json = path === '/api/auth/me'
      ? { username: 'test', role: 'operator', allowed_features: null }
      : path === '/api/jobs'
        ? [{ id: 'job.+', version: '1' }, { id: 'other', version: '1' }]
        : path === '/api/groups'
          ? { groups: [{ name: 'group-a' }] }
          : path === '/api/agents'
            ? [{ pc_id: 'PC.+' }, { pc_id: 'other' }, { pc_id: 'PC-2' }]
            : [];
    await route.fulfill({ json });
  });
});

for (const mode of ['all', 'groups', 'pcs']) {
  test(`Exec ${mode} keeps the accepted execution after form changes and navigates only on click`, async ({ mount, page }) => {
    let release!: () => void;
    const pending = new Promise<void>((resolve) => { release = resolve; });
    let submitted: unknown;
    await page.route('**/api/exec/*', async (route) => {
      submitted = route.request().postDataJSON();
      await pending;
      await route.fulfill({ json: { job_id: 'job.+', exec_id: 'exec-1', target_count: 2, subjects: ['target'] } });
    });
    const c = await mount(<ExecutionActivityHost initial="/exec?job_id=job.%2B&pc=PC-1" />);
    await c.locator('#exec-target').selectOption(mode);
    if (mode === 'groups') {
      await c.locator('#exec-groups').fill('group-a');
      await c.locator('#exec-groups').press('Enter');
    }
    if (mode === 'pcs') {
      await c.locator('#exec-pcs').fill('PC-2');
      await c.getByRole('option', { name: 'PC-2', exact: true }).click();
    }
    await c.getByRole('button', { name: 'POST /api/exec/job.+' }).click();
    await page.getByRole('dialog').getByRole('button', { name: mode === 'all' ? 'Exec on all agents' : 'Exec', exact: true }).click();
    await expect.poll(() => submitted).toEqual({ target: {
      all: mode === 'all', groups: mode === 'groups' ? ['group-a'] : [], pcs: mode === 'pcs' ? ['PC-1', 'PC-2'] : [],
    } });
    await expect(c.getByRole('link', { name: 'View in Activity' })).toHaveCount(0);
    if (mode === 'pcs') await c.getByRole('button', { name: 'Remove PC-1', exact: true }).click();
    if (mode === 'groups') await c.getByRole('button', { name: 'Remove group-a', exact: true }).click();
    await c.locator('#exec-job').selectOption('other');
    await c.locator('#exec-target').selectOption('pcs');
    release();
    const link = c.getByRole('link', { name: 'View in Activity' });
    await expect(link).toHaveAttribute('href', activityUrl({ jobId: 'job.+', execId: 'exec-1' }));
    await c.locator('#exec-job').selectOption('');
    await expect(link).toHaveAttribute('href', activityUrl({ jobId: 'job.+', execId: 'exec-1' }));
    await expect(c.getByTestId('location')).toHaveText('/exec?job_id=job.%2B&pc=PC-1');
    const request = page.waitForRequest((r) => r.url().includes('/api/results?'));
    await link.click();
    const params = new URL((await request).url()).searchParams;
    expect(params.get('exec_id')).toBe('^exec-1$');
    expect(params.get('job_id')).toBe('^job\\.\\+$');
    expect(params.has('status')).toBe(false);
    expect(params.get('limit')).toBe('50');
    expect(params.has('since')).toBe(true);
    await expect(c.getByText('No activity matches', { exact: true })).toBeVisible();
  });
}

for (const execId of ['run.+', '']) {
  test(`Run uses submitted PC and ${execId ? 'execution ID' : 'documented PC fallback'}`, async ({ mount, page }) => {
    let release!: () => void;
    const pending = new Promise<void>((resolve) => { release = resolve; });
    let submitted: unknown;
    await page.route('**/api/run', async (route) => {
      submitted = route.request().postDataJSON();
      await pending;
      await route.fulfill({ json: {
        pc_id: 'PC.+', exec_id: execId || undefined, request_id: 'request', exit_code: 1,
        stdout: '', stderr: 'failure', started_at: '2026-01-01', finished_at: '2026-01-01',
      } });
    });
    const c = await mount(<ExecutionActivityHost initial="/run" />);
    await c.getByPlaceholder('pc-01', { exact: true }).fill('PC.+');
    await c.getByRole('option', { name: 'PC.+', exact: true }).click();
    await c.getByPlaceholder('ad-hoc-…').fill(execId);
    await c.locator('textarea').fill('exit 1');
    await c.getByRole('button', { name: 'POST /api/run' }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Run', exact: true }).click();
    await expect.poll(() => submitted).toMatchObject({ pc_id: 'PC.+', script: 'exit 1' });
    await c.getByPlaceholder('pc-01', { exact: true }).fill('other');
    await c.getByRole('option', { name: 'other', exact: true }).click();
    await c.getByPlaceholder('ad-hoc-…').fill('other');
    release();
    const link = c.getByRole('link', { name: 'View in Activity' });
    await expect(link).toHaveAttribute('href', activityUrl({ jobId: 'adhoc-run', execId, pcId: 'PC.+' }));
    await expect(c.getByTestId('location')).toHaveText('/run');
    if (!execId) await expect(c.getByText(/Without an execution ID/)).toBeVisible();
    await link.click();
    await expect(c.locator('#res-pc')).toHaveValue('^PC\\.\\+$');
    await expect(c.locator('#res-job')).toHaveValue('^adhoc-run$');
    await expect(c.locator('#res-status')).toHaveValue('');
  });
}

test('a rejected submission has no success link', async ({ mount, page }) => {
  await page.route('**/api/exec/*', (route) => route.fulfill({ status: 500, body: 'failed' }));
  const c = await mount(<ExecutionActivityHost initial="/exec?job_id=job.%2B&pc=PC-1" />);
  await c.getByRole('button', { name: 'POST /api/exec/job.+' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Exec', exact: true }).click();
  await expect(c.getByText('Exec failed', { exact: true })).toBeVisible();
  await expect(c.getByRole('link', { name: 'View in Activity' })).toHaveCount(0);
});

test('Activity preserves navigation, back/forward and debounced edits without stale URL writes', async ({ mount, page }) => {
  const requests: URLSearchParams[] = [];
  await page.route('**/api/results?*', async (route) => {
    requests.push(new URL(route.request().url()).searchParams);
    await route.fulfill({ json: [] });
  });
  const original = '/activity?job_id=old-job&pc_id=old-pc&exec_id=old-exec';
  const c = await mount(<ExecutionActivityHost initial={original} />);
  await expect(c.locator('#res-exec')).toHaveValue('old-exec');
  await c.locator('#res-exec').fill('uncommitted');
  await c.getByRole('link', { name: 'Other execution' }).click();
  await expect(c.locator('#res-exec')).toHaveValue('new-exec');
  await page.waitForTimeout(650);
  await expect(c.getByTestId('location')).toHaveText('/activity?job_id=new-job&pc_id=new-pc&exec_id=new-exec');
  await c.getByRole('button', { name: 'Back', exact: true }).click();
  await expect(c.locator('#res-exec')).toHaveValue('old-exec');
  await page.waitForTimeout(650);
  await expect(c.getByTestId('location')).toHaveText(original);
  await c.getByRole('button', { name: 'Forward', exact: true }).click();
  await expect(c.locator('#res-job')).toHaveValue('new-job');
  await expect(c.locator('#res-pc')).toHaveValue('new-pc');
  await c.locator('#res-exec').fill('edited-exec');
  await expect(c.getByTestId('location')).toContainText('exec_id=edited-exec');
  await expect.poll(() => requests.at(-1)?.get('exec_id')).toBe('edited-exec');
  await c.locator('#res-exec').fill('');
  await expect(c.getByTestId('location')).not.toContainText('exec_id=');
});

test('Activity continues across scan-budget windows with pinned since and resets cursor on filter change', async ({ mount, page }) => {
  const requests: URLSearchParams[] = [];
  await page.route('**/api/results?*', async (route) => {
    const sp = new URL(route.request().url()).searchParams;
    requests.push(sp);
    const cursor = sp.get('cursor');
    const stdout = sp.get('stdout');

    if (stdout === 'fresh-filter') {
      await route.fulfill({
        json: [
          {
            result_id: 'res-fresh-1',
            request_id: 'req-fresh-1',
            exec_id: 'exec-fresh-1',
            pc_id: 'PC-FRESH',
            exit_code: 0,
            skipped: false,
            stdout: 'fresh-filter output',
            stderr: '',
            started_at: '2026-04-02T00:00:00Z',
            finished_at: '2026-04-02T00:00:01Z',
            job_id: 'job-fresh',
            version: '1',
          },
        ],
        headers: {
          'X-Results-Scanned': '12',
          'X-Results-Stop-Reason': 'end_of_range',
        },
      });
      return;
    }

    if (!cursor) {
      await route.fulfill({
        json: [],
        headers: {
          'X-Results-Scanned': '10000',
          'X-Results-Stop-Reason': 'scan_limit_reached',
          'X-Results-Next-Cursor': 'cursor-window-2',
        },
      });
      return;
    }

    if (cursor === 'cursor-window-2') {
      await route.fulfill({
        json: [
          {
            result_id: 'res-old-match',
            request_id: 'req-old-match',
            exec_id: 'exec-old-match',
            pc_id: 'PC-OLD-99',
            exit_code: 0,
            skipped: false,
            stdout: 'older matching run',
            stderr: '',
            started_at: '2026-03-20T12:00:00Z',
            finished_at: '2026-03-20T12:00:05Z',
            job_id: 'job-old',
            version: '1',
          },
        ],
        headers: {
          'X-Results-Scanned': '2500',
          'X-Results-Stop-Reason': 'end_of_range',
        },
      });
      return;
    }

    await route.fulfill({ json: [] });
  });

  const c = await mount(<ExecutionActivityHost initial="/activity?job_id=job-old" />);

  // First window hit the 10k scan cap with 0 matches: must show incomplete-scan
  // state with a continuation button rather than definitive "No activity matches".
  await expect(c.getByText('No matches in the scanned window', { exact: true })).toBeVisible();
  await expect(c.getByTestId('activity-scan-status')).toContainText('10000');

  const searchOlderBtn = c.getByRole('button', { name: 'Search older history', exact: true });
  await expect(searchOlderBtn).toBeVisible();
  await searchOlderBtn.click();

  // Second window finds the older matching row and reports end_of_range.
  await expect(c.getByText('PC-OLD-99', { exact: true })).toBeVisible();
  await expect(c.getByTestId('activity-scan-status')).toContainText('End of time range (2500 runs inspected)');
  expect(requests.length).toBeGreaterThanOrEqual(2);
  expect(requests[1]?.get('cursor')).toBe('cursor-window-2');
  // `since` must stay pinned across cursor steps in the same search series.
  expect(requests[1]?.get('since')).toBe(requests[0]?.get('since'));

  // Stepping back returns to the previous window; changing a filter resets cursor.
  await c.getByRole('button', { name: 'Previous window', exact: true }).click();
  await expect(c.getByText('No matches in the scanned window', { exact: true })).toBeVisible();
  await c.getByRole('button', { name: 'Search older history', exact: true }).click();
  await expect(c.getByText('PC-OLD-99', { exact: true })).toBeVisible();

  await c.locator('#res-stdout').fill('fresh-filter');
  await expect(c.getByText('PC-FRESH', { exact: true })).toBeVisible();
  expect(requests.at(-1)?.get('stdout')).toBe('fresh-filter');
  expect(requests.at(-1)?.has('cursor')).toBe(false);
});
