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

type DisplayField = {
  field: string;
  label: string;
  type?: 'number' | 'bytes' | 'timestamp';
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

function fmtTimestamp(v: unknown): string {
  if (typeof v !== 'string') return String(v ?? '—');
  const d = new Date(v);
  return isNaN(d.getTime()) ? v : d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

function renderCell(value: unknown, kind?: string): string {
  if (value === null || value === undefined) return '—';
  switch (kind) {
    case 'bytes':     return fmtBytes(value);
    case 'number':    return fmtNumber(value);
    case 'timestamp': return fmtTimestamp(value);
    default:
      if (typeof value === 'object') return JSON.stringify(value);
      return String(value);
  }
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
                  {columns.map((c) => (
                    <TableCell key={c.field}>
                      <code className="text-xs">{renderCell(r.facts[c.field], c.type)}</code>
                    </TableCell>
                  ))}
                  <TableCell className="text-muted text-xs">
                    {r.collected_at ? new Date(r.collected_at).toLocaleString() : '—'}
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
                  collected {fact.collected_at ? new Date(fact.collected_at).toLocaleString() : '—'}
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
                          <TableCell className="text-muted">{d.label}</TableCell>
                          <TableCell><code className="text-xs">{renderCell(factsObj[d.field], d.type)}</code></TableCell>
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
