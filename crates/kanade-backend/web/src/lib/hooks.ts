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
