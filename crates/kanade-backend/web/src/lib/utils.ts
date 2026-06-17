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
 * Split a bulk-entry string (pasted from Excel / a text editor, or
 * typed) into a clean token list: split on commas **and** any
 * whitespace (spaces, tabs, newlines), trim, drop empties. Lets the
 * pickers accept `pc01,pc02,pc03`, `pc01, pc02,  pc03`, and
 * tab/newline-separated columns all the same. Does NOT dedupe — the
 * caller merges against its existing selection.
 */
export function splitTokens(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/**
 * Escape a string for safe interpolation into a `RegExp` / a regex
 * query param — the pickers build a `^(a|b|c)$` alternation out of
 * pasted ids to existence-check them server-side, and an id with a
 * regex metachar (`.`, `+`, …) would otherwise change the pattern's
 * meaning.
 */
export function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
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
 * Render a PC's last-seen account for a table cell. Prefers the
 * friendly display name, falls back to the `DOMAIN\user` login name,
 * and shows both as `Display Name (DOMAIN\user)` when they differ —
 * the shape an operator needs to actually identify and contact whoever
 * uses a machine. Blank / non-string inputs collapse to `—`.
 *
 * Inputs are `unknown` because the cross-PC inventory-search rows carry
 * the account under dynamic `@account_*` keys read off a
 * `Record<string, unknown>`; the typed fleet-list row passes
 * `string | null`, which `unknown` also accepts.
 */
export function fmtAccount(displayName: unknown, user: unknown): string {
  const dn = typeof displayName === 'string' && displayName.trim() ? displayName : null;
  const u = typeof user === 'string' && user.trim() ? user : null;
  if (dn && u && dn !== u) return `${dn} (${u})`;
  return dn ?? u ?? '—';
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

/**
 * Compare two dotted version strings numerically: `true` when `a` is
 * strictly newer than `b` (so "0.43.10" > "0.43.9", which a plain
 * string compare gets wrong). Missing / non-numeric components count
 * as 0, so a malformed version sorts low rather than throwing.
 */
export function isVersionNewer(a: string, b: string): boolean {
  const pa = a.split('.').map((n) => Number.parseInt(n, 10) || 0);
  const pb = b.split('.').map((n) => Number.parseInt(n, 10) || 0);
  const len = Math.max(pa.length, pb.length);
  for (let i = 0; i < len; i++) {
    const x = pa[i] ?? 0;
    const y = pb[i] ?? 0;
    if (x !== y) return x > y;
  }
  return false;
}

/**
 * The still-UNRESOLVED quarantine entries for an agent: versions the
 * boot sentinel rolled back that are NEWER than the version the agent
 * currently runs. A rolled-back version older than what the agent now
 * runs successfully is resolved — the host has since adopted a newer
 * build — so it should not raise an alert (otherwise a 3000-host fleet
 * would carry stale quarantine badges forever, with no way to clear
 * them short of per-host `unquarantine`). When the agent version is
 * unknown, every entry is treated as unresolved (we can't prove it
 * healed).
 */
export function unresolvedQuarantine(
  quarantined: string[] | undefined,
  agentVersion: string | null | undefined,
): string[] {
  if (!quarantined || quarantined.length === 0) return [];
  if (!agentVersion) return quarantined;
  return quarantined.filter((v) => isVersionNewer(v, agentVersion));
}
