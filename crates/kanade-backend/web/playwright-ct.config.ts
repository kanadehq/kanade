import path from 'node:path';
import { fileURLToPath } from 'node:url';

import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig, devices } from '@playwright/experimental-ct-react';

// The package is `"type": "module"`, so Playwright loads this config as ESM
// and `__dirname` is undefined — derive it from the module URL instead.
const rootDir = path.dirname(fileURLToPath(import.meta.url));

// Component tests (`*.ct.tsx` — see the `testMatch` note below) render real
// React components in a real browser via Playwright CT. This exists for the
// class of bug unit tests
// (`bun test`) structurally cannot see — layout, stacking order, and whether
// an element can be hit-tested at all (Issue #1094). The pure-logic suite
// stays in `bun test`; keep only genuinely browser-dependent assertions here.
export default defineConfig({
  testDir: './src',
  // `*.ct.tsx`, deliberately NOT `*.ct.spec.tsx`: `bun test` (the unit suite)
  // globs `*.spec.*` / `*.test.*`, so a `.spec.` name would make it try to run
  // these browser tests as unit tests and fail. The `.ct.` marker keeps the
  // two suites cleanly partitioned.
  testMatch: /.*\.ct\.tsx$/,
  // Serve the component bundle through the same Vite plugins the app builds
  // with, so mounted components resolve `@/…` imports and get the exact
  // Tailwind stylesheet prod ships — the layout the hit-testing asserts on.
  use: {
    ctViteConfig: {
      plugins: [react(), tailwindcss()],
      resolve: {
        alias: { '@': path.resolve(rootDir, './src') },
      },
    },
  },
  // Chromium only: hit-testing and stacking are engine-agnostic here, and a
  // single pinned engine keeps CI fast and the (future) screenshot baselines
  // stable. Widen if a cross-engine rendering difference ever needs guarding.
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  // Local runs surface the HTML report; CI just needs the pass/fail line.
  reporter: process.env.CI ? 'line' : 'html',
});
