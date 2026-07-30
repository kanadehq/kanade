/// True when `error` is a failed dynamic import of a route chunk —
/// the stale-tab-after-redeploy case that route-level code splitting
/// (#1215③) makes possible: the browser holds an old `index.html`
/// whose asset hashes the redeployed backend no longer serves, so a
/// lazy `import()` on the next navigation 404s. Each browser reports
/// it differently:
///   - Chromium: TypeError "Failed to fetch dynamically imported module: …"
///   - Firefox:  TypeError "error loading dynamically imported module: …"
///   - WebKit:   "Importing a module script failed." / ChunkLoadError
///     (webpack's name, kept for safety if the toolchain ever changes)
export function isChunkLoadError(error: unknown): boolean {
  if (!(error instanceof Error)) return false;
  if (error.name === 'ChunkLoadError') return true;
  return /dynamically imported module|Importing a module script failed/i.test(error.message);
}
