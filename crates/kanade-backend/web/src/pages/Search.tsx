import { useQuery } from '@tanstack/react-query';
import { Loader2, Search } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { cn, fmtIsoLocal } from '@/lib/utils';

/** v0.35 / #87: per-explode-column descriptor returned by
 *  `GET /api/inventory/jobs`. The SPA uses `kind` to pick the right
 *  filter widget (text input vs numeric comparator dropdown). */
type ExplodeColumn = {
  field: string;
  /** SQLite affinity declared on the manifest. Defaults to `text`
   *  when omitted server-side. */
  type?: 'text' | 'integer' | 'real';
  index?: boolean;
};

type ExplodeSpec = {
  field: string;
  table: string;
  primary_key: string[];
  columns: ExplodeColumn[];
  track_history?: boolean;
};

type InventoryJob = {
  manifest_id: string;
  description: string | null;
  display: unknown[];
  summary: unknown[] | null;
  explode: ExplodeSpec[] | null;
};

/** Operators per column type. Text columns get LIKE-style matchers;
 *  numeric columns get the SQL comparators. `eq` is always available
 *  as the bare `<col>=<v>` form. */
type Op = 'eq' | 'contains' | 'prefix' | 'lt' | 'le' | 'gt' | 'ge' | 'ne';

const TEXT_OPS: Op[] = ['eq', 'contains', 'prefix', 'ne'];
const NUMERIC_OPS: Op[] = ['eq', 'lt', 'le', 'gt', 'ge', 'ne'];

const OP_LABEL: Record<Op, string> = {
  eq: '=',
  contains: 'contains',
  prefix: 'starts with',
  lt: '<',
  le: '≤',
  gt: '>',
  ge: '≥',
  ne: '≠',
};

const DEFAULT_LIMIT = 100;
const MAX_LIMIT = 5000;

type Filter = {
  /** Stable client-side id so React's keyed list stays sane across
   *  add / remove. Backend gets a `<col><op>=<v>` query param. */
  uid: number;
  column: string;
  op: Op;
  value: string;
};

type ResultRow = Record<string, unknown> & {
  pc_id?: unknown;
  collected_at?: unknown;
};

function columnKind(c: ExplodeColumn): 'text' | 'numeric' {
  return c.type === 'integer' || c.type === 'real' ? 'numeric' : 'text';
}

function opsForColumn(c: ExplodeColumn): Op[] {
  return columnKind(c) === 'numeric' ? NUMERIC_OPS : TEXT_OPS;
}

function filterToParam(f: Filter): [string, string] | null {
  if (!f.column || f.value === '') return null;
  const key = f.op === 'eq' ? f.column : `${f.column}__${f.op}`;
  return [key, f.value];
}

function formatCell(v: unknown): string {
  if (v === null || v === undefined) return '—';
  if (typeof v === 'string') return v;
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  return JSON.stringify(v);
}

/** v0.35 / #87: cross-PC inventory search page. Drives
 *  `GET /api/inventory/{manifest_id}/search/{field}` programmatically
 *  so operators don't have to know the Django-style URL syntax.
 *
 *  Originally named `Software` because the motivating use case was
 *  "find PCs with X app installed", but `explode:` manifests can
 *  flatten any array (disks / network adapters / services / …) so
 *  the page covers fleet-wide inventory search in general — renamed
 *  to match. */
export function InventorySearch() {
  const jobsQ = useQuery({
    queryKey: ['inventory-jobs'],
    queryFn: () => apiFetch<InventoryJob[]>('/api/inventory/jobs'),
  });

  // Only jobs with at least one explode spec are searchable.
  const searchableJobs = useMemo(
    () => (jobsQ.data ?? []).filter((j) => (j.explode?.length ?? 0) > 0),
    [jobsQ.data],
  );

  const [manifestId, setManifestId] = useState<string>('');
  const [field, setField] = useState<string>('');
  const [filters, setFilters] = useState<Filter[]>([]);
  const [limit, setLimit] = useState(DEFAULT_LIMIT);
  const [offset, setOffset] = useState(0);
  // Gemini #116 fix: monotonic counter for filter UIDs. `Date.now() +
  // prev.length` collides when two adds happen in the same ms (e.g.
  // operator-paste of many filters or future programmatic adds).
  // A ref-backed counter is fragility-free and survives re-renders.
  const filterUidCounter = useRef(0);

  // Auto-pick the first searchable manifest / first field when the
  // jobs list first lands or the operator picks a manifest whose
  // field set changed.
  useEffect(() => {
    if (!manifestId && searchableJobs.length > 0) {
      setManifestId(searchableJobs[0].manifest_id);
    }
  }, [manifestId, searchableJobs]);

  const currentJob = useMemo(
    () => searchableJobs.find((j) => j.manifest_id === manifestId) ?? null,
    [searchableJobs, manifestId],
  );
  const currentSpec = useMemo(
    () => currentJob?.explode?.find((s) => s.field === field) ?? null,
    [currentJob, field],
  );

  useEffect(() => {
    const specs = currentJob?.explode ?? [];
    if (specs.length === 0) {
      setField('');
      return;
    }
    if (!specs.some((s) => s.field === field)) {
      setField(specs[0].field);
      setFilters([]);
      setOffset(0);
    }
  }, [currentJob, field]);

  // Build the search query URL. `null` when not enough state to fire.
  const searchUrl = useMemo(() => {
    if (!manifestId || !field) return null;
    const sp = new URLSearchParams();
    for (const f of filters) {
      const param = filterToParam(f);
      if (param) sp.append(param[0], param[1]);
    }
    sp.set('limit', String(limit));
    if (offset > 0) sp.set('offset', String(offset));
    return `/api/inventory/${encodeURIComponent(manifestId)}/search/${encodeURIComponent(field)}?${sp.toString()}`;
  }, [manifestId, field, filters, limit, offset]);

  const searchQ = useQuery({
    queryKey: ['inventory-search', searchUrl],
    queryFn: () => apiFetch<ResultRow[]>(searchUrl!),
    enabled: !!searchUrl,
  });

  const rows = searchQ.data ?? [];
  const columns = currentSpec?.columns ?? [];

  function addFilter() {
    if (columns.length === 0) return;
    const col = columns[0];
    setFilters((prev) => [
      ...prev,
      {
        uid: ++filterUidCounter.current,
        column: col.field,
        op: opsForColumn(col)[0],
        value: '',
      },
    ]);
    setOffset(0);
  }

  function updateFilter(uid: number, patch: Partial<Filter>) {
    setFilters((prev) =>
      prev.map((f) => {
        if (f.uid !== uid) return f;
        const merged = { ...f, ...patch };
        // If the column changed, re-validate the op against the new
        // column's kind — operator likely doesn't want `<` carried
        // over onto a text column.
        if (patch.column) {
          const newCol = columns.find((c) => c.field === patch.column);
          if (newCol && !opsForColumn(newCol).includes(merged.op)) {
            merged.op = opsForColumn(newCol)[0];
          }
        }
        return merged;
      }),
    );
    setOffset(0);
  }

  function removeFilter(uid: number) {
    setFilters((prev) => prev.filter((f) => f.uid !== uid));
    setOffset(0);
  }

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Search className="size-5" />
            Inventory search
          </CardTitle>
          <CardDescription>
            Cross-PC search across any inventory manifest's <code>explode</code> field
            (#40). Filter by column, page through up to {MAX_LIMIT} rows at a time.
            Click a row to jump to that PC's inventory detail.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {jobsQ.isLoading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="size-4 animate-spin" /> loading manifests…
            </div>
          ) : jobsQ.error ? (
            <ErrorCard title="Couldn't load manifest list" error={jobsQ.error} />
          ) : searchableJobs.length === 0 ? (
            <div className="text-sm text-muted">
              No inventory manifests have an <code>explode</code> spec yet. Add one to a
              manifest YAML (see <code>configs/jobs/inventory-sw.yaml</code> for an
              example) and register it with{' '}
              <code>kanade job create</code>.
            </div>
          ) : (
            <>
              <div className="flex flex-wrap items-end gap-3">
                <div className="flex flex-col gap-1">
                  <Label htmlFor="software-manifest">manifest</Label>
                  <Select
                    id="software-manifest"
                    value={manifestId}
                    onChange={(e) => {
                      setManifestId(e.target.value);
                      setFilters([]);
                      setOffset(0);
                    }}
                  >
                    {searchableJobs.map((j) => (
                      <option key={j.manifest_id} value={j.manifest_id}>
                        {j.manifest_id}
                      </option>
                    ))}
                  </Select>
                </div>
              </div>

              {(currentJob?.explode?.length ?? 0) > 1 ? (
                <div className="inline-flex rounded-md border border-border overflow-hidden text-sm">
                  {(currentJob?.explode ?? []).map((s) => (
                    <button
                      key={s.field}
                      onClick={() => {
                        setField(s.field);
                        setFilters([]);
                        setOffset(0);
                      }}
                      className={cn(
                        'px-3 py-1.5 transition-colors',
                        field === s.field
                          ? 'bg-fg/10 text-fg font-medium'
                          : 'text-muted hover:bg-fg/5 hover:text-fg',
                      )}
                    >
                      {s.field}
                    </button>
                  ))}
                </div>
              ) : null}

              <div className="space-y-2">
                <div className="flex items-center gap-2">
                  <Label className="text-xs text-muted">filters</Label>
                  <Button size="sm" variant="secondary" onClick={addFilter} disabled={columns.length === 0}>
                    + add filter
                  </Button>
                  {filters.length > 0 ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => {
                        // CodeRabbit #116 fix: clearing filters
                        // should also reset pagination to the first
                        // page — otherwise operator can end up
                        // looking at offset=200 of a 50-row
                        // unfiltered result set with an empty table.
                        setFilters([]);
                        setOffset(0);
                      }}
                    >
                      clear all
                    </Button>
                  ) : null}
                </div>
                {filters.length === 0 ? (
                  <div className="text-xs text-muted">
                    No filters yet — results show up to {limit} rows from the start.
                  </div>
                ) : (
                  <div className="space-y-2">
                    {filters.map((f) => {
                      const col = columns.find((c) => c.field === f.column) ?? columns[0];
                      const ops = opsForColumn(col);
                      return (
                        <div key={f.uid} className="flex flex-wrap items-center gap-2">
                          <Select
                            value={f.column}
                            onChange={(e) => updateFilter(f.uid, { column: e.target.value })}
                            className="w-40"
                          >
                            {columns.map((c) => (
                              <option key={c.field} value={c.field}>
                                {c.field}
                                {c.type ? ` (${c.type})` : ''}
                              </option>
                            ))}
                          </Select>
                          <Select
                            value={f.op}
                            onChange={(e) => updateFilter(f.uid, { op: e.target.value as Op })}
                            className="w-32"
                          >
                            {ops.map((o) => (
                              <option key={o} value={o}>
                                {OP_LABEL[o]}
                              </option>
                            ))}
                          </Select>
                          <Input
                            value={f.value}
                            onChange={(e) => updateFilter(f.uid, { value: e.target.value })}
                            placeholder="value"
                            type={columnKind(col) === 'numeric' ? 'number' : 'text'}
                            className="w-56"
                          />
                          <Button size="sm" variant="ghost" onClick={() => removeFilter(f.uid)}>
                            ✕
                          </Button>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            </>
          )}
        </CardContent>
      </Card>

      {currentSpec ? (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">
              {currentJob?.manifest_id} · {currentSpec.field}
            </CardTitle>
            <CardDescription>
              <code>{currentSpec.table}</code> · columns:{' '}
              {currentSpec.columns.map((c, i) => (
                <span key={c.field}>
                  {i > 0 ? ', ' : ''}
                  <code>{c.field}</code>
                  {c.type ? <span className="text-xs"> ({c.type})</span> : null}
                </span>
              ))}
            </CardDescription>
          </CardHeader>
          <CardContent>
            {searchQ.isLoading ? (
              <div className="flex items-center gap-2 text-muted text-sm">
                <Loader2 className="size-4 animate-spin" /> searching…
              </div>
            ) : searchQ.error ? (
              <ErrorCard title="Search failed" error={searchQ.error} />
            ) : rows.length === 0 ? (
              <div className="text-sm text-muted py-4">
                No results match these filters.
              </div>
            ) : (
              <>
                <div className="text-xs text-muted mb-2">
                  Showing rows {offset + 1}–{offset + rows.length}
                  {rows.length === limit ? ` (page size = ${limit}; more may be available)` : ''}
                </div>
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>pc_id</TableHead>
                      <TableHead>collected_at</TableHead>
                      {currentSpec.columns.map((c) => (
                        <TableHead key={c.field}>{c.field}</TableHead>
                      ))}
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {rows.map((row) => {
                      const pcId = formatCell(row.pc_id);
                      // Gemini #116 fix: derive a row key from the
                      // spec's primary_key tuple (= the SQL unique
                      // identity per (pc_id, job_id)) instead of
                      // index-based `${pcId}-${i}`. Index keys are a
                      // React anti-pattern under pagination /
                      // filter changes — switching pages reuses
                      // indices and breaks reconciliation. The PK
                      // tuple is what the backend already considers
                      // unique within (pc_id, job_id), so combining
                      // pc_id + those values yields a globally
                      // stable key for the result set.
                      const rowKey = [
                        pcId,
                        ...currentSpec.primary_key.map((k) => formatCell(row[k])),
                      ].join('|');
                      // Gemini #116 fix: pass row.collected_at
                      // through to fmtIsoLocal directly. The
                      // previous `fmtIsoLocal(formatCell(...))`
                      // round-tripped null through formatCell's
                      // dash placeholder, which fmtIsoLocal then
                      // re-tagged as the same dash via its own
                      // null branch — fine, but inscrutable.
                      const collectedAt =
                        typeof row.collected_at === 'string' ? row.collected_at : null;
                      return (
                        <TableRow key={rowKey}>
                          <TableCell>
                            <Link
                              to={`/inventory?pc=${encodeURIComponent(pcId)}`}
                              className="underline hover:text-fg"
                            >
                              {pcId}
                            </Link>
                          </TableCell>
                          <TableCell className="text-muted text-xs">
                            {fmtIsoLocal(collectedAt)}
                          </TableCell>
                          {currentSpec.columns.map((c) => (
                            <TableCell key={c.field}>{formatCell(row[c.field])}</TableCell>
                          ))}
                        </TableRow>
                      );
                    })}
                  </TableBody>
                </Table>
                <div className="flex items-center gap-2 mt-3 text-xs">
                  <Label htmlFor="software-limit" className="text-muted">
                    page size
                  </Label>
                  <Select
                    id="software-limit"
                    value={String(limit)}
                    onChange={(e) => {
                      setLimit(Number(e.target.value));
                      setOffset(0);
                    }}
                    className="w-24"
                  >
                    {[50, 100, 250, 500, 1000].map((n) => (
                      <option key={n} value={n}>
                        {n}
                      </option>
                    ))}
                  </Select>
                  <Button
                    size="sm"
                    variant="secondary"
                    onClick={() => setOffset(Math.max(0, offset - limit))}
                    disabled={offset === 0}
                  >
                    ← prev
                  </Button>
                  <Button
                    size="sm"
                    variant="secondary"
                    onClick={() => setOffset(offset + limit)}
                    disabled={rows.length < limit}
                    title={
                      rows.length < limit
                        ? 'Last page (fewer rows than the page size returned)'
                        : 'Next page'
                    }
                  >
                    next →
                  </Button>
                </div>
              </>
            )}
          </CardContent>
        </Card>
      ) : null}
    </div>
  );
}
