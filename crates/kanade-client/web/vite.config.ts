// Vite config for the Kanade Client App's WebView.
// Port matches `tauri.conf.json::build.devUrl` so `tauri dev`
// picks it up automatically.

import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  // Tauri expects the build output at this directory; matches
  // `tauri.conf.json::build.frontendDist` (`../web/dist`).
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
