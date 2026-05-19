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
 * Format an ISO-8601 timestamp for the table cells / detail pages.
 * Replaces `T` with a space and strips the sub-second `Z` suffix so
 * the result reads like `2026-05-19 14:27:48Z`. Pass-through for
 * inputs that don't parse (e.g. legacy strings the backend may emit
 * once we add new fields). `null` collapses to `—`.
 *
 * Existing pages (Agents, Audit, Inventory, Rollout) still carry
 * their own copies — pre-dating this helper. Migrating them is
 * tracked as a follow-up cleanup PR.
 */
export function fmtIsoLocal(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}
