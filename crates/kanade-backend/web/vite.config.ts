import path from 'node:path';

import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
      // monaco-editor 0.56 added an `exports` map:
      //
      //   "./*.js": "./esm/vs/*.js",
      //   "./*":    "./esm/vs/*.js"
      //
      // so the deep specifier `monaco-editor/esm/vs/...` now expands to
      // `./esm/vs/esm/vs/...` — the prefix is applied twice and the path
      // doesn't exist. `monaco-worker-manager` (a dependency of monaco-yaml)
      // still imports the pre-0.56 form, which fails the bundle outright:
      //
      //   Rolldown failed to resolve import
      //   "monaco-editor/esm/vs/editor/editor.worker.js"
      //   from "node_modules/monaco-worker-manager/worker.js"
      //
      // Rewrite that one specifier to the 0.56 public form, which resolves
      // to exactly the same file. Aliased to a package specifier rather than
      // an absolute path so it doesn't depend on where the package is
      // hoisted to.
      //
      // Pinning monaco-editor back to 0.55 was the first attempt and does
      // not hold: renovate treats an exact pin as an outdated one and opens
      // a PR to move it forward again (it did, within minutes — #1101).
      // Fixing the incompatibility is the version that survives.
      //
      // Remove once monaco-worker-manager ships a build using the new form.
      // `monaco-worker-manager@2.0.1` is the version this was written for,
      // and it has exactly one such import — verified by grepping the
      // package, so this alias is not hiding others.
      'monaco-editor/esm/vs/editor/editor.worker.js':
        'monaco-editor/editor/editor.worker.js',
    },
  },
  build: {
    outDir: 'dist',
    // Keep an asset hash so cached browsers pick up new builds
    // automatically. rust-embed reads dist/ at compile time and
    // doesn't care about file names.
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    // Allow Tailscale MagicDNS hosts (`<host>.<tailnet>.ts.net`) so
    // phones reach the dev server over Tailscale without each operator
    // adding their personal tailnet here. IPs are always allowed, so
    // 100.x and LAN access keep working with no extra entry.
    allowedHosts: ['.ts.net'],
    proxy: {
      // Dev backend runs on :8081 (see `cargo make backend-dev`,
      // which uses backend.dev.toml). The production KanadeBackend
      // Windows service stays on :8080, so dev + service can coexist.
      // Override BACKEND_PROXY at run time if you'd rather aim Vite
      // at a different host / port (e.g., a staging machine).
      // `ws: true` is required for #1140's remote viewer: without it the
      // proxy passes normal requests but drops the `Upgrade` handshake, so
      // `/api/remote/<pc>/ws` fails in dev while working in production —
      // the worst shape of bug to meet while building a viewer. Vite's own
      // HMR socket is on a separate path, so this does not collide with it.
      '/api': { target: process.env.BACKEND_PROXY ?? 'http://localhost:8081', ws: true },
      '/health': process.env.BACKEND_PROXY ?? 'http://localhost:8081',
    },
  },
});
