import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';

/**
 * shadcn-style class merger: clsx for conditional joins,
 * tailwind-merge to dedupe conflicting Tailwind atoms (e.g.
 * `cn("p-2", "p-4")` keeps only `p-4`).
 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * Format an ISO-8601 timestamp for the table cells / detail pages in
 * the **browser's local timezone**, shaped like `2026-05-19 14:27:48`
 * (no `Z` / offset — the operator's wall clock is the implicit zone).
 * Pass-through for inputs that don't parse (e.g. legacy strings the
 * backend may emit once we add new fields). `null` collapses to `—`.
 *
 * The earlier implementation used `toISOString()` here, which is
 * specified to render in UTC — so despite the function name the
 * output was UTC for every page. Pages that wanted UTC explicitly
 * never existed (all callers want operator-local), so the fix is to
 * actually localise rather than rename.
 */
// Module-scope so it isn't re-allocated on every fmtIsoLocal call;
// fmtIsoLocal renders the timestamp cell in nearly every table.
const pad = (n: number) => String(n).padStart(2, '0');

export function fmtIsoLocal(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  );
}
