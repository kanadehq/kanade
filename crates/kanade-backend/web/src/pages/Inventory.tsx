import { useQuery } from '@tanstack/react-query';
import { Loader2, ScrollText } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useSearchParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

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
              Ship a probe with{' '}
              <code>kanade schedule create configs/schedules/hourly-inventory.yaml</code>
              {' '}from the repo, or roll your own in <code>configs/jobs/</code>.
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
        (factsQ.data ?? []).map((fact) => {
          const factsObj = (fact.facts ?? {}) as Record<string, unknown>;
          return (
            <Card key={fact.job_id}>
              <CardHeader>
                <CardTitle className="flex items-center gap-2">
                  <ScrollText className="size-5 text-violet" />
                  <code className="text-sm">{fact.job_id}</code>
                </CardTitle>
                <CardDescription>
                  collected {fmtIsoLocal(fact.collected_at)}
                </CardDescription>
              </CardHeader>
              <CardContent>
                {fact.display.length === 0 ? (
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
                )}
              </CardContent>
            </Card>
          );
        })
      )}
    </div>
  );
}
