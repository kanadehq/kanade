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

/**
 * Heartbeat freshness window. An agent whose `last_heartbeat` falls
 * within this window counts as **online**; anything older (or never
 * heard from) is **offline / stale**.
 *
 * Mirrors the backend `STALE_THRESHOLD` (`api/health.rs`, 2 min) and
 * the Dashboard fleet-health rollup, so the "active / known" tile on
 * the Dashboard and the per-row online/offline badge on the Agents
 * page always agree on which hosts are connected. Heartbeats cadence
 * at ~30 s, so 2 min absorbs a few missed ticks without flapping.
 */
export const AGENT_ACTIVE_THRESHOLD_MS = 2 * 60 * 1000;

/** True when `last_heartbeat` is fresh enough to call the agent
 *  online. Single source of truth shared by the Dashboard and the
 *  Agents list — see {@link AGENT_ACTIVE_THRESHOLD_MS}.
 *
 *  Pass `referenceTime` (a `Date.now()` snapshot captured once at the
 *  top of a render) when calling this repeatedly in one pass — e.g.
 *  the Agents list computes counts, filters, and renders per-row
 *  badges off the same predicate, and a shared `now` keeps all three
 *  in agreement for an agent sitting exactly on the threshold. */
export function isAgentOnline(
  lastHeartbeat: string | null | undefined,
  referenceTime: number = Date.now(),
): boolean {
  if (!lastHeartbeat) return false;
  const ts = new Date(lastHeartbeat).getTime();
  if (isNaN(ts)) return false;
  return referenceTime - ts < AGENT_ACTIVE_THRESHOLD_MS;
}
