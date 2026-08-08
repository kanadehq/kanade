import { useEffect, useState } from 'react';

/**
 * Returns `value` delayed by `delay` ms. Each change resets the timer,
 * so a fast typist only triggers one downstream effect (a fetch, a URL
 * sync) once they pause. Extracted from the per-page copies in
 * Activity / Events so the shared PcPicker — and future consumers —
 * reuse one implementation.
 */
export function useDebouncedValue<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), delay);
    return () => clearTimeout(id);
  }, [value, delay]);
  return debounced;
}

/**
 * Live `window.matchMedia(query).matches`.
 *
 * Exists because a couple of behaviours can't be expressed in CSS alone:
 * the data tables collapse into cards below a breakpoint (index.css), and
 * the column-resize machinery in ui/table.tsx must render *nothing* — no
 * handles, no `<colgroup>`, no inline width — while that's the case. A
 * media query can hide a handle, but it can't stop React from emitting an
 * inline `width` that would then out-specify the card-mode stylesheet.
 *
 * Seeded synchronously from `matchMedia` so the first paint is already
 * correct (no flash of the wrong layout). Guarded for environments without
 * `matchMedia` (jsdom-less unit runners), where it reports `false`.
 */
export function useMediaQuery(query: string): boolean {
  const supported = typeof window !== 'undefined' && typeof window.matchMedia === 'function';
  const [matches, setMatches] = useState(() => (supported ? window.matchMedia(query).matches : false));
  useEffect(() => {
    if (!supported) return;
    const mql = window.matchMedia(query);
    // Re-read on subscribe: the viewport can change between the initial
    // render and this effect (a fast resize, a devtools dock).
    setMatches(mql.matches);
    const onChange = (e: MediaQueryListEvent) => setMatches(e.matches);
    mql.addEventListener('change', onChange);
    return () => mql.removeEventListener('change', onChange);
  }, [query, supported]);
  return matches;
}
