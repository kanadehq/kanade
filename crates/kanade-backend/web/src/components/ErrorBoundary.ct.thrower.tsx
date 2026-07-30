/// Playwright CT helpers ("test stories") for `ErrorBoundary.ct.tsx`.
/// Two constraints force this shape:
///   - components in a mounted JSX tree must live OUTSIDE the test
///     file (the CT harness serialises the tree and re-resolves each
///     component reference by module), and
///   - an `Error` can't cross that same bridge as a prop — it arrives
///     as a plain object, fails `instanceof Error`, and the boundary
///     under test takes the wrong branch. So each story constructs
///     its error HERE, in the browser, at render time.
/// Nothing in the app imports this; it is dead code outside CT.
export function ChunkThrower(): never {
  throw new TypeError('Failed to fetch dynamically imported module: http://x/assets/Jobs-a.js');
}

export function GenericThrower(): never {
  throw new Error('boom');
}
