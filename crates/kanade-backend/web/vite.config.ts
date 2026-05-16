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
    proxy: {
      // Backend runs on 8080 in dev; proxy /api/* + /health so the
      // SPA can call them from the Vite dev server without CORS.
      '/api': 'http://localhost:8080',
      '/health': 'http://localhost:8080',
    },
  },
});
