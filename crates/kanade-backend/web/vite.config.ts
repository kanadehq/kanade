import path from 'node:path';

import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
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
      '/api': process.env.BACKEND_PROXY ?? 'http://localhost:8081',
      '/health': process.env.BACKEND_PROXY ?? 'http://localhost:8081',
    },
  },
});
