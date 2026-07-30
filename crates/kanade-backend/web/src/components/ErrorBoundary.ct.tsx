import { expect, test } from '@playwright/experimental-ct-react';

import { ErrorBoundary } from './ErrorBoundary';
import { ChunkThrower, GenericThrower } from './ErrorBoundary.ct.thrower';

// #1215③ review: route-level code splitting made a stale-chunk import
// failure (old tab + redeployed backend) a routine render-time throw.
// These mount the real boundary in a real browser so the containment
// — fallback INSIDE the content area, not a blank app — is what
// breaks if the boundary regresses, not just a mocked predicate.

test.describe('ErrorBoundary', () => {
  test('a chunk-load failure renders the update-and-reload prompt', async ({ mount }) => {
    const c = await mount(
      <ErrorBoundary>
        <ChunkThrower />
      </ErrorBoundary>,
    );
    await expect(c.getByText('A new version is available')).toBeVisible();
    await expect(c.getByRole('button', { name: 'Reload' })).toBeVisible();
    // The generic retry is withheld on purpose — retrying cannot fix
    // a chunk the server no longer has; only a reload can.
    await expect(c.getByRole('button', { name: 'Retry' })).toHaveCount(0);
  });

  test('a generic render error is contained and shows the message', async ({ mount }) => {
    const c = await mount(
      <ErrorBoundary>
        <GenericThrower />
      </ErrorBoundary>,
    );
    await expect(c.getByText('Something went wrong rendering this page')).toBeVisible();
    await expect(c.getByText('boom')).toBeVisible();
    await expect(c.getByRole('button', { name: 'Reload' })).toBeVisible();
    await expect(c.getByRole('button', { name: 'Retry' })).toBeVisible();
  });

  test('children render untouched when nothing throws', async ({ mount }) => {
    const c = await mount(
      <ErrorBoundary>
        <p>fine</p>
      </ErrorBoundary>,
    );
    await expect(c.getByText('fine')).toBeVisible();
    await expect(c.getByRole('button', { name: 'Reload' })).toHaveCount(0);
  });
});
