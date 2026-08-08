import { expect, test } from '@playwright/experimental-ct-react';

import { ChipGroupHarness } from './Events.chipGroups.harness';

// Issue #1342: the Events filter chips are folded into groups. The
// promise the fold makes is that hiding chips never hides the FILTER —
// the active selection stays on screen whatever is collapsed. That is a
// statement about what is actually visible in a laid-out browser, which
// is precisely what `bun test` cannot answer (#1094): jsdom has no layout
// engine, so an element pushed off-screen or collapsed to zero height
// still reads as present. The grouping arithmetic itself is unit-tested
// in `lib/vocabGroups.test.ts`; only the visibility claims live here.

const KINDS = [
  'active', 'agent_offline', 'agent_online', 'agent_update', 'app_sample',
  'boot', 'command_signature_absent', 'command_signature_ok',
  'command_signature_unknown_key', 'command_signature_unprovisioned',
  'idle', 'lock', 'log_service_started', 'log_service_stopped', 'logoff',
  'logon', 'presence', 'resume', 'shutdown', 'sleep', 'unexpected_shutdown',
  'unlock', 'web_visit',
];

const SOURCES = [
  'agent:idle_sampler', 'agent:self_update', 'agent:startup', 'app-usage',
  'attendance-snapshot', 'backend:heartbeat-watchdog', 'command_signature',
  'web-history:brave', 'web-history:chrome', 'web-history:edge',
  'winlog:Security', 'winlog:System',
];

test.describe('collapsed by default', () => {
  test('individual chips are not on screen until a group is opened', async ({ mount }) => {
    const c = await mount(<ChipGroupHarness values={KINDS} />);
    // The whole point of the issue: 23 chips no longer occupy the page.
    await expect(c.getByRole('button', { name: /^unexpected_shutdown:/ })).toHaveCount(0);
    await expect(c.getByRole('button', { name: /expand power/ })).toBeVisible();
  });

  test('opening a group reveals its members', async ({ mount }) => {
    const c = await mount(<ChipGroupHarness values={KINDS} />);
    await c.getByRole('button', { name: /expand power/ }).click();
    await expect(c.getByRole('button', { name: /^unexpected_shutdown:/ })).toBeVisible();
    // Only that group opens — a fold is per-group, not a global toggle.
    await expect(c.getByRole('button', { name: /^logon:/ })).toHaveCount(0);
  });
});

test.describe('the selection is never hidden by the fold', () => {
  // The core promise. A collapsed group holding a selection must not be
  // able to leave the operator looking at a filtered result set with no
  // visible reason for it.
  test('a selected chip stays visible while its group is collapsed', async ({ mount }) => {
    const c = await mount(<ChipGroupHarness values={KINDS} initialInc={['unexpected_shutdown']} />);
    await expect(c.getByRole('button', { name: /expand power/ })).toBeVisible();
    const chip = c.getByRole('button', { name: 'unexpected_shutdown: included' });
    await expect(chip).toBeVisible();
    await expect(chip).toBeInViewport();
  });

  test('a collapsed group carrying a selection does not render as untouched', async ({ mount }) => {
    const c = await mount(<ChipGroupHarness values={KINDS} initialInc={['boot']} />);
    // `power` holds one of five — it must say "partly selected", not sit
    // silently in the off state while quietly constraining the query.
    await expect(c.getByRole('button', { name: 'power: partly selected' })).toBeVisible();
  });

  // PR #1346 review (CodeRabbit + claude): `splitCsv` dedupes `kinds` and
  // `kinds_ex` separately, so a URL naming the same value in both put it
  // in `inc` AND `exc`. Concatenating them rendered it twice with the same
  // React key. Asserted in the browser because "how many chips are on
  // screen" is the symptom an operator would actually see.
  test('a value included and excluded at once renders exactly one chip', async ({ mount }) => {
    const c = await mount(
      <ChipGroupHarness values={KINDS} initialInc={['retired_kind']} initialExc={['retired_kind']} />,
    );
    await expect(c.getByRole('button', { name: /^retired_kind: / })).toHaveCount(1);
  });

  test('a selection for a value no longer in the vocabulary is still shown', async ({ mount }) => {
    // Hand-edited URL, or a kind the fleet stopped emitting. It still
    // filters, so it must remain visible and clearable — otherwise the
    // page silently returns nothing with no control to undo it.
    const c = await mount(<ChipGroupHarness values={KINDS} initialInc={['retired_kind']} />);
    await expect(c.getByRole('button', { name: 'retired_kind: included' })).toBeVisible();
  });
});

test.describe('group headers act on the whole family', () => {
  test('one click includes every member, and the URL gets individual values', async ({ mount }) => {
    const c = await mount(<ChipGroupHarness values={KINDS} />);
    await c.getByRole('button', { name: 'command_signature: not filtered' }).click();
    // Individual values, never the category name — a category in the URL
    // would change meaning whenever the grouping rules were edited.
    await expect(c.getByTestId('inc')).toHaveText(
      'command_signature_absent,command_signature_ok,' +
        'command_signature_unknown_key,command_signature_unprovisioned',
    );
  });

  test('a second click excludes the family', async ({ mount }) => {
    const c = await mount(<ChipGroupHarness values={KINDS} />);
    const header = c.getByRole('button', { name: /^command_signature: / });
    await header.click();
    await header.click();
    await expect(c.getByTestId('inc')).toHaveText('');
    await expect(c.getByTestId('exc')).toContainText('command_signature_absent');
  });

  test('clearing removes everything in one action', async ({ mount }) => {
    const c = await mount(
      <ChipGroupHarness values={KINDS} initialInc={['boot']} initialExc={['logon']} />,
    );
    await c.getByRole('button', { name: 'clear' }).click();
    await expect(c.getByTestId('inc')).toHaveText('');
    await expect(c.getByTestId('exc')).toHaveText('');
  });
});

test.describe('source namespaces', () => {
  test('members drop the prefix already shown on the header', async ({ mount }) => {
    const c = await mount(<ChipGroupHarness values={SOURCES} vocabulary="sources" />);
    await c.getByRole('button', { name: /expand web-history/ }).click();
    // `web-history:brave` reads as `brave` under the `web-history` header.
    await expect(c.getByRole('button', { name: 'brave: not filtered' })).toBeVisible();
  });

  test('the header still writes the fully-qualified value', async ({ mount }) => {
    // Shortening is display-only: the filter the backend receives has to
    // stay the real source string.
    const c = await mount(<ChipGroupHarness values={SOURCES} vocabulary="sources" />);
    await c.getByRole('button', { name: 'winlog: not filtered' }).click();
    await expect(c.getByTestId('inc')).toHaveText('winlog:Security,winlog:System');
  });
});
