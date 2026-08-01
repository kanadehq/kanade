// Vite config for the Kanade Client App's WebView.
// Port matches `tauri.conf.json::build.devUrl` so `tauri dev`
// picks it up automatically.

import path from "node:path";

import { defineConfig } from "vite";

// `cargo make demo-client` sets this. The app reaches the outside world
// only through `@tauri-apps/api`'s `invoke` / `listen` — there is no HTTP
// layer to repoint, unlike the backend SPA's `BACKEND_PROXY` — so the
// demo swaps those two modules for shims under `demo/`, and the app runs
// unmodified in a plain browser.
//
// Gated on an env var rather than on `mode` so a normal `bun run dev`
// (what `tauri dev` invokes) can never pick the shims up: the aliases
// are simply absent from the config unless the demo task asked for them,
// and nothing under `demo/` is reachable from the product entry point.
const DEMO = process.env.KANADE_CLIENT_DEMO === "1";

export default defineConfig({
  clearScreen: false,
  resolve: DEMO
    ? {
        alias: {
          "@tauri-apps/api/core": path.resolve(__dirname, "demo/tauri-core.ts"),
          "@tauri-apps/api/event": path.resolve(__dirname, "demo/tauri-event.ts"),
          // Also aliased so the core alias stops at our boundary: this
          // plugin's own bundle imports `addPluginListener` from
          // `@tauri-apps/api/core`, and redirecting that to the shim
          // broke the build on a missing export from a module the app
          // never imports directly.
          "@tauri-apps/plugin-notification": path.resolve(
            __dirname,
            "demo/tauri-notification.ts",
          ),
        },
      }
    : {},
  server: {
    // Browser demo takes a distinct port so it can coexist with a real
    // `tauri dev` — 1420 is pinned by `tauri.conf.json::build.devUrl`.
    //
    // The app-window demo overrides it back to 1420, because there the
    // dev server IS what `devUrl` points at.
    port: DEMO ? Number(process.env.KANADE_CLIENT_DEMO_PORT ?? 1421) : 1420,
    strictPort: true,
  },
  // Tauri expects the build output at this directory; matches
  // `tauri.conf.json::build.frontendDist` (`../web/dist`).
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
