import { useQuery } from '@tanstack/react-query';
import { Loader2, ScrollText } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useSearchParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { cn, fmtIsoLocal } from '@/lib/utils';

type DisplayField = {
  field: string;
  label: string;
  /** v0.30 / #39: `table` renders a nested sub-table on the PC
   *  detail page using the field's array-of-objects value + the
   *  `columns` schema. Fleet summary view falls back to a count
   *  for table fields so the wide list stays compact. */
  type?: 'number' | 'bytes' | 'timestamp' | 'table';
  /** Required when `type === 'table'`. Each column is itself a
   *  DisplayField so nested cells reuse the same render hints. */
  columns?: DisplayField[];
};

type InventoryFact = {
  job_id: string;
  facts: Record<string, unknown>;
  display: DisplayField[];
  summary: DisplayField[] | null;
  collected_at: string | null;
  recorded_at: string | null;
};

type InventoryJob = {
  manifest_id: string;
  description: string | null;
  display: DisplayField[];
  summary: DisplayField[] | null;
};

type InventoryRow = {
  pc_id: string;
  facts: Record<string, unknown>;
  collected_at: string | null;
};

type InventoryByJob = {
  manifest_id: string;
  display: DisplayField[];
  summary: DisplayField[] | null;
  rows: InventoryRow[];
};

/** v0.34 / #92: one row from `inventory_history` — populated by the
 *  projector's diff step (#41 / #86) and served by
 *  `GET /api/inventory/{manifest_id}/history/pc/{pc_id}`. */
type HistoryEventRow = {
  id: number;
  pc_id: string;
  job_id: string;
  field_path: string;
  /** JSON of the spec's primary_key tuple, e.g. `{"key": "{ABC-...}",
   *  "source": "x64"}` for an inventory_sw row. Null for scalar
   *  history (future scope — array history only in v1). */
  identity_json: string | null;
  change_kind: 'added' | 'removed' | 'changed';
  /** Full row snapshot before the change. Null on `added`. */
  before_json: string | null;
  /** Full row snapshot after. Null on `removed`. */
  after_json: string | null;
  observed_at: string | null;
};

type ChangeKindFilter = 'any' | 'added' | 'removed' | 'changed';

const SINCE_PRESETS: Array<{ value: string; label: string; ms: number | null }> = [
  { value: '24h', label: 'last 24h', ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  label: 'last 7d',  ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', label: 'last 30d', ms: 30 * 24 * 60 * 60 * 1000 },
  { value: '90d', label: 'last 90d', ms: 90 * 24 * 60 * 60 * 1000 },
  { value: 'all', label: 'all time', ms: null },
];

const CHANGE_KIND_FILTERS: ChangeKindFilter[] = ['any', 'added', 'removed', 'changed'];

function fmtBytes(v: unknown): string {
  const n = typeof v === 'number' ? v : Number(v);
  if (!Number.isFinite(n) || n <= 0) return String(v ?? '—');
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let i = 0;
  let x = n;
  while (x >= 1024 && i < units.length - 1) { x /= 1024; i++; }
  return `${x.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function fmtNumber(v: unknown): string {
  const n = typeof v === 'number' ? v : Number(v);
  if (!Number.isFinite(n)) return String(v ?? '—');
  return n.toLocaleString();
}

function renderCell(value: unknown, kind?: string): string {
  if (value === null || value === undefined) return '—';
  switch (kind) {
    case 'bytes':     return fmtBytes(value);
    case 'number':    return fmtNumber(value);
    case 'timestamp': return typeof value === 'string' ? fmtIsoLocal(value) : String(value);
    default:
      if (typeof value === 'object') return JSON.stringify(value);
      return String(value);
  }
}

/// v0.30 / #39: scalar-cell helper used both at the top level of
/// the PC-detail value column AND inside nested table cells.
/// Returns a React node (so the empty-state span gets muted
/// styling, not a literal `—` rendered as code).
function ScalarCell({ value, kind }: { value: unknown; kind?: string }) {
  if (value === null || value === undefined) {
    return <span className="text-muted text-xs">—</span>;
  }
  return <code className="text-xs">{renderCell(value, kind)}</code>;
}

/// v0.30 / #39: nested sub-table renderer used by PcDetail when a
/// DisplayField carries `type: 'table'`. Walks `columns` recursively
/// via ScalarCell so child cells reuse the same `bytes`/`number`/
/// `timestamp` formatters as the top-level table.
///
/// Renders nothing when `value` is not an array (operator typo on
/// the manifest? show a muted dash); empty arrays render an empty-
/// state row so operators distinguish "0 disks reported" from
/// "this field isn't an array".
function NestedTable({ value, columns }: { value: unknown; columns: DisplayField[] }) {
  if (!Array.isArray(value)) {
    return <span className="text-muted text-xs">— (expected an array)</span>;
  }
  if (value.length === 0) {
    return <span className="text-muted text-xs">— (none)</span>;
  }
  return (
    <Table>
      <TableHeader>
        <TableRow>
          {columns.map((c) => (
            <TableHead key={c.field}>{c.label}</TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        {value.map((row, i) => {
          const obj = (row ?? {}) as Record<string, unknown>;
          return (
            <TableRow key={i}>
              {columns.map((c) => (
                <TableCell key={c.field}>
                  {/* Gemini #84 medium fix: true recursion. A nested
                      column can itself be `type: 'table'`, so the
                      sub-table renderer must branch the same way
                      PcDetail does — otherwise nested-of-nested
                      (e.g. `disks[].partitions[]`) collapsed to
                      JSON.stringify in the cell. */}
                  {c.type === 'table' && c.columns ? (
                    <NestedTable value={obj[c.field]} columns={c.columns} />
                  ) : (
                    <ScalarCell value={obj[c.field]} kind={c.type} />
                  )}
                </TableCell>
              ))}
            </TableRow>
          );
        })}
      </TableBody>
    </Table>
  );
}

export function Inventory() {
  const [search, setSearch] = useSearchParams();
  const initialPc = search.get('pc') ?? '';
  const [pcId, setPcId] = useState(initialPc);

  useEffect(() => {
    if (pcId) {
      setSearch({ pc: pcId }, { replace: true });
    } else if (search.has('pc')) {
      const next = new URLSearchParams(search);
      next.delete('pc');
      setSearch(next, { replace: true });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pcId]);

  const jobsQ = useQuery({
    queryKey: ['inventory-jobs'],
    queryFn: () => apiFetch<InventoryJob[]>('/api/inventory/jobs'),
    refetchInterval: 60_000,
  });

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Inventory</h2>
        <Badge variant="violet">
          {(jobsQ.data ?? []).length} probe{(jobsQ.data ?? []).length === 1 ? '' : 's'} configured
        </Badge>
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="inv-pc">view</Label>
            <Select id="inv-pc" value={pcId} onChange={(e) => setPcId(e.target.value)}>
              <option value="">fleet (all PCs)</option>
              {pcId && <option value={pcId}>{pcId}</option>}
            </Select>
          </div>
          <div className="space-y-1 text-xs text-muted self-end pb-2">
            Probes are operator-defined PowerShell jobs tagged with an{' '}
            <code>inventory:</code> section in their YAML manifest. Click a row in
            the fleet view (below) to drill into one PC's full facts.
          </div>
        </CardContent>
      </Card>

      {jobsQ.isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />loading probes…
        </div>
      ) : jobsQ.error ? (
        <ErrorCard title="Couldn't load inventory jobs" error={jobsQ.error} />
      ) : (jobsQ.data ?? []).length === 0 ? (
        <Card>
          <CardHeader>
            <CardTitle>No inventory probes configured</CardTitle>
            <CardDescription>
              Ship the bundled probes with{' '}
              <code>kanade schedule create configs/schedules/inventory-hw.yaml</code>
              {' '}and{' '}
              <code>kanade schedule create configs/schedules/inventory-sw.yaml</code>
              {' '}(hardware + installed-software snapshots), or roll your own in{' '}
              <code>configs/jobs/</code>.
            </CardDescription>
          </CardHeader>
        </Card>
      ) : pcId ? (
        <PcDetail pcId={pcId} clear={() => setPcId('')} />
      ) : (
        <FleetView jobs={jobsQ.data ?? []} pickPc={setPcId} />
      )}
    </div>
  );
}

/// Fleet view: per probe, render a horizontal table with rows = PCs
/// and columns = the manifest's `summary` (or `display` fallback).
function FleetView({
  jobs,
  pickPc,
}: {
  jobs: InventoryJob[];
  pickPc: (pc: string) => void;
}) {
  return (
    <div className="space-y-6">
      {jobs.map((j) => (
        <FleetProbeTable key={j.manifest_id} job={j} pickPc={pickPc} />
      ))}
    </div>
  );
}

function FleetProbeTable({
  job,
  pickPc,
}: {
  job: InventoryJob;
  pickPc: (pc: string) => void;
}) {
  const byJob = useQuery({
    queryKey: ['inventory-by-job', job.manifest_id],
    queryFn: () =>
      apiFetch<InventoryByJob>(`/api/inventory/by-job/${encodeURIComponent(job.manifest_id)}`),
    refetchInterval: 30_000,
  });

  const columns = job.summary ?? job.display;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <ScrollText className="size-5 text-violet" />
          <code className="text-sm">{job.manifest_id}</code>
          {job.description && (
            <span className="text-xs text-muted">— {job.description}</span>
          )}
        </CardTitle>
        <CardDescription>
          {(byJob.data?.rows ?? []).length} PC{(byJob.data?.rows ?? []).length === 1 ? '' : 's'} reported · click a row for full facts
        </CardDescription>
      </CardHeader>
      <CardContent>
        {byJob.isLoading ? (
          <div className="flex items-center gap-2 text-muted">
            <Loader2 className="size-4 animate-spin" />loading…
          </div>
        ) : byJob.error ? (
          <ErrorCard title={`Couldn't load facts for ${job.manifest_id}`} error={byJob.error} />
        ) : (byJob.data?.rows ?? []).length === 0 ? (
          <div className="text-muted text-sm">
            No facts yet. Trigger one with <code>kanade exec &lt;job-id&gt;</code>{' '}
            or wait for the next scheduled tick.
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>pc_id</TableHead>
                {columns.map((c) => (
                  <TableHead key={c.field}>{c.label}</TableHead>
                ))}
                <TableHead className="text-muted text-xs">collected</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {byJob.data!.rows.map((r) => (
                <TableRow
                  key={r.pc_id}
                  className="cursor-pointer hover:bg-muted/5"
                  onClick={() => pickPc(r.pc_id)}
                >
                  <TableCell><code className="text-xs">{r.pc_id}</code></TableCell>
                  {columns.map((c) => {
                    // Gemini #84 medium fix: extract once. `Array.isArray`
                    // acts as a type guard so the inner accesses don't
                    // need the `as unknown[]` cast repeated.
                    const val = r.facts[c.field];
                    return (
                      <TableCell key={c.field}>
                        {/* v0.30 / #39: fleet summary cells must
                            stay compact (one row per PC, many
                            columns). For `type: table` collapse to
                            a row count instead of expanding the
                            nested table inline — operator drills
                            into the PC detail view to see the full
                            sub-table. */}
                        {c.type === 'table' ? (
                          <code className="text-xs">
                            {Array.isArray(val)
                              ? `${val.length} row${val.length === 1 ? '' : 's'}`
                              : '—'}
                          </code>
                        ) : (
                          <code className="text-xs">{renderCell(val, c.type)}</code>
                        )}
                      </TableCell>
                    );
                  })}
                  <TableCell className="text-muted text-xs">
                    {fmtIsoLocal(r.collected_at)}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </CardContent>
    </Card>
  );
}

/// Detail view: vertical "field / value" per probe for one PC.
function PcDetail({ pcId, clear }: { pcId: string; clear: () => void }) {
  const factsQ = useQuery({
    queryKey: ['inventory-facts', pcId],
    queryFn: () => apiFetch<InventoryFact[]>(`/api/inventory/${encodeURIComponent(pcId)}`),
    refetchInterval: 30_000,
  });

  return (
    <div className="space-y-4">
      <div className="text-sm">
        <button onClick={clear} className="underline text-muted hover:text-fg">
          ← back to fleet view
        </button>
      </div>

      {factsQ.isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />loading inventory for {pcId}…
        </div>
      ) : factsQ.error ? (
        <ErrorCard title={`Couldn't load inventory for ${pcId}`} error={factsQ.error} />
      ) : (factsQ.data ?? []).length === 0 ? (
        <Card>
          <CardHeader>
            <CardTitle>No facts yet for <code>{pcId}</code></CardTitle>
            <CardDescription>
              The agent hasn't run any inventory-tagged job successfully yet.
            </CardDescription>
          </CardHeader>
        </Card>
      ) : (
        (factsQ.data ?? []).map((fact) => (
          <FactCard key={fact.job_id} fact={fact} pcId={pcId} />
        ))
      )}
    </div>
  );
}

/** v0.34 / #92: per-card tab strip — `Now` is the v0.30 display table,
 *  `History` is the new timeline of `inventory_history` events for
 *  this PC + this manifest. State is local to each card so an operator
 *  can be on different tabs across different probes simultaneously. */
function FactCard({ fact, pcId }: { fact: InventoryFact; pcId: string }) {
  const [tab, setTab] = useState<'now' | 'history'>('now');
  const factsObj = (fact.facts ?? {}) as Record<string, unknown>;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <ScrollText className="size-5 text-violet" />
          <code className="text-sm">{fact.job_id}</code>
        </CardTitle>
        <CardDescription>
          collected {fmtIsoLocal(fact.collected_at)}
        </CardDescription>
        <div className="mt-2 inline-flex rounded-md border border-border overflow-hidden text-xs">
          {(['now', 'history'] as const).map((t) => (
            <button
              key={t}
              onClick={() => setTab(t)}
              className={cn(
                'px-3 py-1 transition-colors',
                tab === t
                  ? 'bg-fg/10 text-fg font-medium'
                  : 'text-muted hover:bg-fg/5 hover:text-fg',
              )}
            >
              {t === 'now' ? 'Now' : 'History'}
            </button>
          ))}
        </div>
      </CardHeader>
      <CardContent>
        {tab === 'now' ? (
          fact.display.length === 0 ? (
            <details>
              <summary className="cursor-pointer text-muted text-xs">raw JSON</summary>
              <pre className="text-xs whitespace-pre-wrap break-words mt-2 bg-muted/5 p-2 rounded">
                {JSON.stringify(fact.facts, null, 2)}
              </pre>
            </details>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>field</TableHead>
                  <TableHead>value</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {fact.display.map((d) => (
                  <TableRow key={d.field}>
                    <TableCell className="text-muted align-top">{d.label}</TableCell>
                    <TableCell>
                      {/* v0.30 / #39: nested sub-table for
                          `type: table` (e.g. inventory-hw's
                          `disks: [{ device_id, size_bytes,
                          ... }]`). Scalars still use
                          ScalarCell so the empty-state dash
                          gets muted styling. */}
                      {d.type === 'table' && d.columns ? (
                        <NestedTable value={factsObj[d.field]} columns={d.columns} />
                      ) : (
                        <ScalarCell value={factsObj[d.field]} kind={d.type} />
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )
        ) : (
          <HistoryPane manifestId={fact.job_id} pcId={pcId} />
        )}
      </CardContent>
    </Card>
  );
}

/** v0.34 / #92: timeline of `inventory_history` events for one
 *  (manifest, pc) pair. Filters by `since` preset and `change_kind`
 *  on the client (the API only takes `since`); kind is rare enough
 *  in volume that a client-side filter is cheaper than another HTTP
 *  param. */
function HistoryPane({ manifestId, pcId }: { manifestId: string; pcId: string }) {
  const [since, setSince] = useState('7d');
  const [kind, setKind] = useState<ChangeKindFilter>('any');
  // v0.34 / #92 follow-up: hw + sw inventory rarely changes day-to-
  // day, so most events the operator cares about are version bumps
  // (`changed`), not the noisier `added` / `removed` from a fresh
  // install or rollout. `diffOnly` is a quick checkbox that locks
  // the kind filter to `changed` without forcing the operator to
  // reach for the dropdown each time.
  const [diffOnly, setDiffOnly] = useState(false);
  const effectiveKind: ChangeKindFilter = diffOnly ? 'changed' : kind;

  // Gemini #113 fix: compute the sliding-window `since` lower bound
  // INSIDE queryFn so each refetch (every 60 s while the History tab
  // is mounted) uses `Date.now()` from that moment, not the moment
  // the operator first picked `7d`. Without this, an operator who
  // leaves the History tab open for hours keeps fetching the same
  // frozen window and stops seeing new events. The queryKey uses the
  // `since` preset value (not the computed ISO string) so the cache
  // partitions cleanly per preset without invalidating on every
  // millisecond tick.
  const historyQ = useQuery({
    queryKey: ['inventory-history', manifestId, pcId, since],
    queryFn: () => {
      const preset = SINCE_PRESETS.find((p) => p.value === since);
      const sinceIso = preset?.ms
        ? new Date(Date.now() - preset.ms).toISOString()
        : null;
      const sp = new URLSearchParams();
      if (sinceIso) sp.set('since', sinceIso);
      const qs = sp.toString();
      return apiFetch<HistoryEventRow[]>(
        `/api/inventory/${encodeURIComponent(manifestId)}/history/pc/${encodeURIComponent(pcId)}${
          qs ? `?${qs}` : ''
        }`,
      );
    },
    refetchInterval: 60_000,
  });

  const filteredRows = useMemo(() => {
    const rows = historyQ.data ?? [];
    return effectiveKind === 'any'
      ? rows
      : rows.filter((r) => r.change_kind === effectiveKind);
  }, [historyQ.data, effectiveKind]);

  // Group events by local-date YYYY-MM-DD so the timeline reads as a
  // collapsible per-day stack (matches `Audit`'s rough density without
  // requiring a heavier <details> per row by default).
  const grouped = useMemo(() => {
    const byDay = new Map<string, HistoryEventRow[]>();
    for (const r of filteredRows) {
      const day = r.observed_at
        ? new Date(r.observed_at).toLocaleDateString()
        : '(no timestamp)';
      const bucket = byDay.get(day) ?? [];
      bucket.push(r);
      byDay.set(day, bucket);
    }
    return Array.from(byDay.entries());
  }, [filteredRows]);

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-end gap-3">
        <div className="flex flex-col gap-1">
          <Label htmlFor={`history-since-${manifestId}`} className="text-xs text-muted">
            since
          </Label>
          <Select
            id={`history-since-${manifestId}`}
            value={since}
            onChange={(e) => setSince(e.target.value)}
          >
            {SINCE_PRESETS.map((p) => (
              <option key={p.value} value={p.value}>
                {p.label}
              </option>
            ))}
          </Select>
        </div>
        <div className="flex flex-col gap-1">
          <Label htmlFor={`history-kind-${manifestId}`} className="text-xs text-muted">
            kind
          </Label>
          <Select
            id={`history-kind-${manifestId}`}
            value={kind}
            onChange={(e) => setKind(e.target.value as ChangeKindFilter)}
            disabled={diffOnly}
          >
            {CHANGE_KIND_FILTERS.map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </Select>
        </div>
        <label
          className="flex items-center gap-2 pb-2 text-xs text-muted cursor-pointer select-none"
          title="Hide added / removed events, show only field-level changes (= kind: changed)."
        >
          <input
            type="checkbox"
            checked={diffOnly}
            onChange={(e) => setDiffOnly(e.target.checked)}
            className="size-3.5 accent-fg"
          />
          diff only
        </label>
      </div>

      {historyQ.isLoading ? (
        <div className="flex items-center gap-2 text-muted text-sm">
          <Loader2 className="size-4 animate-spin" />loading history…
        </div>
      ) : historyQ.error ? (
        <ErrorCard title="Couldn't load history" error={historyQ.error} />
      ) : grouped.length === 0 ? (
        <div className="text-sm text-muted py-4">
          No history yet for this PC + filter.
        </div>
      ) : (
        <div className="space-y-4">
          {grouped.map(([day, events]) => (
            <details key={day} open className="border border-border rounded">
              <summary className="cursor-pointer px-3 py-2 text-sm font-medium bg-muted/5">
                {day} <span className="text-muted font-normal">· {events.length} event{events.length === 1 ? '' : 's'}</span>
              </summary>
              <div className="divide-y divide-border">
                {events.map((e) => (
                  <HistoryEventRowView key={e.id} event={e} />
                ))}
              </div>
            </details>
          ))}
        </div>
      )}
    </div>
  );
}

/** v0.34 / #92: single row in the history timeline. Renders a
 *  change-kind badge + field path + identity tuple + the relevant
 *  before/after snippet, with a `details` expand for the full JSON. */
function HistoryEventRowView({ event }: { event: HistoryEventRow }) {
  const identity = useMemo(() => {
    if (!event.identity_json) return null;
    try {
      return JSON.parse(event.identity_json) as Record<string, unknown>;
    } catch {
      return null;
    }
  }, [event.identity_json]);

  const before = useMemo(
    () => parseJsonObject(event.before_json),
    [event.before_json],
  );
  const after = useMemo(
    () => parseJsonObject(event.after_json),
    [event.after_json],
  );

  const diffPairs = useMemo(() => {
    if (event.change_kind !== 'changed' || !before || !after) return null;
    const keys = new Set([...Object.keys(before), ...Object.keys(after)]);
    const pairs: Array<{ field: string; before: unknown; after: unknown }> = [];
    for (const k of keys) {
      if (!Object.is(before[k], after[k])) {
        pairs.push({ field: k, before: before[k], after: after[k] });
      }
    }
    return pairs;
  }, [event.change_kind, before, after]);

  const variant: 'success' | 'danger' | 'amber' =
    event.change_kind === 'added'
      ? 'success'
      : event.change_kind === 'removed'
        ? 'danger'
        : 'amber';

  const snapshot =
    event.change_kind === 'added' ? after : event.change_kind === 'removed' ? before : null;

  return (
    <div className="px-3 py-2 text-sm space-y-1">
      <div className="flex flex-wrap items-center gap-2">
        <Badge variant={variant}>{event.change_kind}</Badge>
        <code className="text-xs text-muted">{event.field_path}</code>
        {identity ? (
          <span className="text-xs">
            {Object.entries(identity).map(([k, v], i) => (
              <span key={k}>
                {i > 0 ? ' · ' : ''}
                <span className="text-muted">{k}:</span>{' '}
                {/* Gemini #113 fix: route identity values through the
                    same formatValue helper the before/after diff and
                    snapshot rows use, so null / undefined / nested
                    objects render consistently across the timeline
                    (was bare `String(v)` which prints `"null"` /
                    `"[object Object]"`). */}
                <code>{formatValue(v)}</code>
              </span>
            ))}
          </span>
        ) : null}
        <span className="ml-auto text-xs text-muted">{fmtIsoLocal(event.observed_at)}</span>
      </div>
      {diffPairs ? (
        <div className="text-xs space-y-0.5">
          {diffPairs.map((p) => (
            <div key={p.field}>
              <span className="text-muted">{p.field}:</span>{' '}
              <code className="line-through text-danger/80">{formatValue(p.before)}</code>
              {' → '}
              <code className="text-success">{formatValue(p.after)}</code>
            </div>
          ))}
        </div>
      ) : snapshot ? (
        <div className="text-xs">
          {Object.entries(snapshot).map(([k, v]) => (
            <span key={k} className="mr-3">
              <span className="text-muted">{k}:</span> <code>{formatValue(v)}</code>
            </span>
          ))}
        </div>
      ) : null}
      <details className="text-xs">
        <summary className="cursor-pointer text-muted">raw JSON</summary>
        <pre className="whitespace-pre-wrap break-words mt-1 bg-muted/5 p-2 rounded">
          {JSON.stringify(
            {
              identity: identity ?? event.identity_json,
              before: before ?? event.before_json,
              after: after ?? event.after_json,
            },
            null,
            2,
          )}
        </pre>
      </details>
    </div>
  );
}

function parseJsonObject(s: string | null): Record<string, unknown> | null {
  if (!s) return null;
  try {
    const parsed = JSON.parse(s) as unknown;
    return typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

function formatValue(v: unknown): string {
  if (v === null || v === undefined) return '—';
  if (typeof v === 'string') return v;
  return JSON.stringify(v);
}
