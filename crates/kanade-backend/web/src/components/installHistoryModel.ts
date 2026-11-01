export interface InstallAttempt {
  id: string;
  issuer: string | null;
  os: string;
  created_at: number;
  expires_at: number;
  last_error_kind?: string | null;
  last_received_at?: number | null;
  status: string;
  last_stage: string | null;
  last_outcome: string | null;
}

export interface InstallEvent {
  seq: number;
  stage: string;
  outcome: string;
  exit_code: number;
  error_kind?: string | null;
  received_at: number;
}

export function newestAttempts(attempts: readonly InstallAttempt[]): InstallAttempt[] {
  return [...attempts].sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
}

export function sortEvents(events: readonly InstallEvent[]): InstallEvent[] {
  return [...events].sort((a, b) => a.seq - b.seq);
}

// Undefined allows initial selection; null preserves an explicit deselection.
// Call only after a successful fetch so failures retain the current selection.
export function reconcileSelection(selected: string | null | undefined, attempts: readonly InstallAttempt[]): string | null | undefined {
  if (selected === undefined) return newestAttempts(attempts)[0]?.id;
  return selected && attempts.some((a) => a.id === selected) ? selected : null;
}

export function formatTimestamp(seconds: number, locale?: string): string {
  return new Date(seconds * 1000).toLocaleString(locale);
}
