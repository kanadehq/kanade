// Stateful host for the `ChipGroupPicker` component tests (#1342).
//
// Lives in its own file, not inside the `.ct.tsx`, because Playwright CT
// compiles the mounted tree through Vite and can only mount components it
// can import — a component declared in the test file itself is not
// reachable. Same reason `ErrorBoundary.ct.thrower.tsx` exists.
//
// The picker is a controlled component: it renders `inc` / `exc` and calls
// back to change them. The real page holds that state in `Events`, so the
// tests need an equivalent owner or every click would be a no-op and the
// interaction assertions would pass against a frozen UI.
import { useState } from 'react';

import { ChipGroupPicker } from './Events';
import { groupKinds, groupSources } from '@/lib/vocabGroups';

export function ChipGroupHarness({
  values,
  vocabulary = 'kinds',
  initialInc = [],
  initialExc = [],
}: {
  values: string[];
  vocabulary?: 'kinds' | 'sources';
  initialInc?: string[];
  initialExc?: string[];
}) {
  const [inc, setInc] = useState<string[]>(initialInc);
  const [exc, setExc] = useState<string[]>(initialExc);
  return (
    <div className="w-[900px] bg-background p-4">
      <ChipGroupPicker
        label={vocabulary}
        values={values}
        inc={inc}
        exc={exc}
        setInc={setInc}
        setExc={setExc}
        group={vocabulary === 'kinds' ? groupKinds : groupSources}
      />
      {/* Mirrors what the page would put on the URL, so a test can assert
          the resulting query without reaching into React internals — and
          so a regression that writes a category name instead of the
          individual values is visible rather than inferred. */}
      <output data-testid="inc">{inc.join(',')}</output>
      <output data-testid="exc">{exc.join(',')}</output>
    </div>
  );
}
