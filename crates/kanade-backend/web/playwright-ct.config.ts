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
        alias: {
          '@': path.resolve(rootDir, './src'),
          // Same alias the app's vite.config carries, and for the same
          // reason: monaco-editor 0.56's `exports` map no longer resolves
          // the deep `esm/vs/...` specifier that `monaco-worker-manager`
          // (via monaco-yaml) still imports. Without it, mounting anything
          // that reaches the YAML editor fails at bundle time rather than
          // in the test. See vite.config.ts for the full write-up.
          'monaco-editor/esm/vs/editor/editor.worker.js':
            'monaco-editor/editor/editor.worker.js',
        },
      },
    },
  },
  // Chromium only: hit-testing and stacking are engine-agnostic here, and a
  // single pinned engine keeps CI fast and the screenshot baselines stable.
  // Widen if a cross-engine rendering difference ever needs guarding.
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  // Screenshot baselines. The `{platform}` segment is load-bearing: font
  // hinting and AA differ per OS, so a Linux baseline can't be matched on
  // win32/darwin. Only the CI platform's (`-linux`) baselines are committed;
  // they are generated in CI (via the `Web` workflow's update path), never
  // locally, since a dev's win32/darwin render would never match the CI run.
  snapshotPathTemplate: '{testDir}/__screenshots__/{arg}-{projectName}-{platform}{ext}',
  expect: {
    // Baseline and comparison run on the SAME pinned CI image (same chromium,
    // same fonts), so an unchanged render diffs to ~zero — the tolerance only
    // has to absorb the odd stray pixel from a runner-image patch bump, not
    // run-to-run noise. So it's kept tight, not generous: `maxDiffPixelRatio`
    // 0.002 still catches a change as small as a single lane's band (a lane is
    // ~15% of the frame, far above 0.2%), and `threshold` 0.1 (down from the
    // 0.2 default) lets a low-alpha hatch-colour shift count — exactly the
    // perceptual regression class this baseline exists to surface (Issue
    // #1094). Lowering the per-pixel threshold is safe here precisely because
    // identical renders produce zero diff regardless of it. If a legitimate
    // runner-image change ever trips this, re-baseline (see web.yml dispatch).
    toHaveScreenshot: { maxDiffPixelRatio: 0.002, threshold: 0.1 },
  },
  // Local runs surface the HTML report; CI just needs the pass/fail line.
  reporter: process.env.CI ? 'line' : 'html',
});
