import { QueryClient, QueryClientProvider } from '@tanstack/react-query';

import i18n from '@/i18n';

import { YamlEditorDialog, type RepoOrigin } from './YamlEditorDialog';

// Pin the language here rather than from the spec: the i18next instance is
// a module singleton and is not on `window`, so the page has no handle to
// switch it. Setting it at import time means the dialog's first render is
// already Japanese and the assertions never race the language change.
void i18n.changeLanguage('ja');

/**
 * Mountable wrappers for the CT test.
 *
 * A separate file because Playwright CT serialises the props it mounts
 * with, so a QueryClient and an `onOpenChange` callback cannot be handed in
 * from the spec — they have to be constructed on the browser side.
 */

// `retry: false` so an unmocked fetch fails immediately instead of the
// dialog sitting on its loading state for the length of the test.
const client = () => new QueryClient({ defaultOptions: { queries: { retry: false } } });

const GIT_ORIGIN: RepoOrigin = {
  path: 'configs/jobs/inventory-basic.yaml',
  repo: 'git@github.com:example/ops-config.git',
  script_file: 'configs/jobs/scripts/inventory-basic.ps1',
};

/** Create mode: the editable branch, no network. */
export function CreateDialog() {
  return (
    <QueryClientProvider client={client()}>
      <YamlEditorDialog open onOpenChange={() => {}} kind="manifest" mode={{ type: 'create' }} />
    </QueryClientProvider>
  );
}

/** Edit mode with Git provenance: the read-only branch and its banner. */
export function GitManagedDialog() {
  return (
    <QueryClientProvider client={client()}>
      <YamlEditorDialog
        open
        onOpenChange={() => {}}
        kind="manifest"
        mode={{ type: 'edit', id: 'inventory-basic' }}
        gitOrigin={GIT_ORIGIN}
      />
    </QueryClientProvider>
  );
}

/** A non-job kind, to prove the noun is per-kind rather than hardcoded. */
export function CreateScheduleDialog() {
  return (
    <QueryClientProvider client={client()}>
      <YamlEditorDialog open onOpenChange={() => {}} kind="schedule" mode={{ type: 'create' }} />
    </QueryClientProvider>
  );
}
