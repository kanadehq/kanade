import { useQuery } from '@tanstack/react-query';
import { Loader2, Search } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { Link } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { useDebouncedValue } from '@/lib/hooks';
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

/** #574: a manifest's `inventory.display` field. Top-level scalar
 *  facts (everything except `type: 'table'`, which is an exploded
 *  array) become searchable columns on the scalar tab. */
type DisplayField = {
  field: string;
  label: string;
  type?: string;
  columns?: DisplayField[];
};

type InventoryJob = {
  manifest_id: string;
  description: string | null;
  display: DisplayField[];
  summary: DisplayField[] | null;
  explode: ExplodeSpec[] | null;
};

/** Operators per column type. Text columns get LIKE-style matchers;
 *  numeric columns get the SQL comparators. `eq` is always available
 *  as the bare `<col>=<v>` form. */
type Op = 'eq' | 'contains' | 'prefix' | 'lt' | 'le' | 'gt' | 'ge' | 'ne';

const TEXT_OPS: Op[] = ['eq', 'contains', 'prefix', 'ne'];
const NUMERIC_OPS: Op[] = ['eq', 'lt', 'le', 'gt', 'ge', 'ne'];

const DEFAULT_LIMIT = 100;
const MAX_LIMIT = 5000;
// #523: same debounce the Activity / Events filter inputs use, so a
// keystroke doesn't fire a fleet-sized exploded-table scan.
const FILTER_DEBOUNCE_MS = 300;

/** #574: sentinel `field` value for the scalar-facts tab. Distinct
 *  from any real explode `field` (which is a plain JSON key, so it
 *  can't contain the leading/trailing double underscores) — picked so
 *  it never collides with an operator's array key. */
const SCALAR_FIELD = '__scalars__';

/** #574: derive the searchable scalar columns from a job's
 *  `display` list — every field EXCEPT the `type: 'table'` ones,
 *  which are arrays handled by the explode sub-table tabs. A
 *  `number` / `bytes` render hint marks the column numeric so it
 *  gets the comparator op set. Mirrors the backend's
 *  `scalar_columns`. */
function scalarColumnsOf(job: InventoryJob | null): ExplodeColumn[] {
  if (!job) return [];
  return (job.display ?? [])
    .filter((d) => d.type !== 'table')
    .map((d) => ({
      field: d.field,
      type: d.type === 'number' || d.type === 'bytes' ? 'integer' : 'text',
    }));
}

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
 *  `GET /api/inventory/{manifest_id}/search/{field}` (explode tabs)
 *  and `GET /api/inventory/{manifest_id}/search-scalars` (#574, the
 *  scalar-facts tab) programmatically so operators don't have to
 *  know the Django-style URL syntax.
 *
 *  Originally named `Software` because the motivating use case was
 *  "find PCs with X app installed", but `explode:` manifests can
 *  flatten any array (disks / network adapters / services / …) and
 *  #574 added top-level scalar search (`os_build` / `pc_model` /
 *  `ram_bytes`), so the page covers fleet-wide inventory search in
 *  general — renamed to match. */
export function InventorySearch() {
  const { t } = useTranslation('search');
  const jobsQ = useQuery({
    queryKey: ['inventory-jobs'],
    queryFn: () => apiFetch<InventoryJob[]>('/api/inventory/jobs'),
  });

  // #574: a job is searchable if it has at least one explode spec OR
  // at least one top-level scalar display field. Pre-#574 only
  // explode-bearing jobs showed up — scalar-only inventory jobs
  // (e.g. an OS-facts probe with no arrays) were invisible here.
  const searchableJobs = useMemo(
    () =>
      (jobsQ.data ?? []).filter(
        (j) => (j.explode?.length ?? 0) > 0 || scalarColumnsOf(j).length > 0,
      ),
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

  // Auto-pick the first searchable manifest when the jobs list first
  // lands.
  useEffect(() => {
    if (!manifestId && searchableJobs.length > 0) {
      setManifestId(searchableJobs[0].manifest_id);
    }
  }, [manifestId, searchableJobs]);

  const currentJob = useMemo(
    () => searchableJobs.find((j) => j.manifest_id === manifestId) ?? null,
    [searchableJobs, manifestId],
  );
  const scalarCols = useMemo(() => scalarColumnsOf(currentJob), [currentJob]);

  // #574: the tab set = one tab per explode field, plus a scalar tab
  // (keyed SCALAR_FIELD) when the manifest has any scalar facts. The
  // tab bar only renders when there's more than one (same rule as
  // pre-#574); a scalar-only manifest shows its form directly.
  const tabs = useMemo(() => {
    const out: { key: string; label: string; isScalar: boolean }[] = (
      currentJob?.explode ?? []
    ).map((s) => ({ key: s.field, label: s.field, isScalar: false }));
    if (scalarCols.length > 0) {
      out.push({ key: SCALAR_FIELD, label: t('scalarTab'), isScalar: true });
    }
    return out;
  }, [currentJob, scalarCols, t]);

  const isScalar = field === SCALAR_FIELD;
  const currentSpec = useMemo(
    () =>
      isScalar ? null : (currentJob?.explode?.find((s) => s.field === field) ?? null),
    [currentJob, field, isScalar],
  );
  // Unified searchable column set for the active tab.
  const columns = isScalar ? scalarCols : (currentSpec?.columns ?? []);
  // Whether there's a tab to render a result view for.
  const hasView = isScalar || !!currentSpec;

  // Keep `field` pointed at a tab that exists on the current manifest.
  useEffect(() => {
    if (tabs.length === 0) {
      setField('');
      return;
    }
    if (!tabs.some((tab) => tab.key === field)) {
      setField(tabs[0].key);
      setFilters([]);
      setOffset(0);
    }
  }, [tabs, field]);

  // #523: debounce the filter values before they reach the queryKey —
  // each keystroke in a filter input previously issued a fresh search
  // request (a scan over fleet-sized exploded tables / facts_json).
  // Same 300 ms the Activity / Events filters use.
  const dFilters = useDebouncedValue(filters, FILTER_DEBOUNCE_MS);

  // Build the search query URL. `null` when not enough state to fire.
  const searchUrl = useMemo(() => {
    if (!manifestId || !field) return null;
    const sp = new URLSearchParams();
    // Empty live filters bypass the debounce: a manifest/field switch
    // calls setFilters([]) synchronously, but dFilters would keep the
    // OLD tab's filters for 300 ms — pairing them with the new tab
    // fired one bogus request (and a flash of wrong data) before the
    // debounce settled (review PR #551, gemini).
    const activeFilters = filters.length === 0 ? filters : dFilters;
    for (const f of activeFilters) {
      const param = filterToParam(f);
      if (param) sp.append(param[0], param[1]);
    }
    sp.set('limit', String(limit));
    if (offset > 0) sp.set('offset', String(offset));
    // #574: scalar tab hits the facts_json endpoint; explode tabs hit
    // the derived-table one keyed by the array `field`.
    const base = isScalar
      ? `/api/inventory/${encodeURIComponent(manifestId)}/search-scalars`
      : `/api/inventory/${encodeURIComponent(manifestId)}/search/${encodeURIComponent(field)}`;
    return `${base}?${sp.toString()}`;
  }, [manifestId, field, filters, dFilters, limit, offset, isScalar]);

  const searchQ = useQuery({
    queryKey: ['inventory-search', searchUrl],
    queryFn: () => apiFetch<ResultRow[]>(searchUrl!),
    enabled: !!searchUrl,
  });

  const rows = searchQ.data ?? [];

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
            {t('title')}
          </CardTitle>
          <CardDescription>
            <Trans
              ns="search"
              i18nKey="description"
              values={{ max: MAX_LIMIT }}
              components={{ code: <code /> }}
            />
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {jobsQ.isLoading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="size-4 animate-spin" /> {t('loadingManifests')}
            </div>
          ) : jobsQ.error ? (
            <ErrorCard title={t('loadError')} error={jobsQ.error} />
          ) : searchableJobs.length === 0 ? (
            <div className="text-sm text-muted">
              <Trans
                ns="search"
                i18nKey="noManifests"
                components={{ code: <code /> }}
              />
            </div>
          ) : (
            <>
              <div className="flex flex-wrap items-end gap-3">
                <div className="flex flex-col gap-1">
                  <Label htmlFor="software-manifest">{t('labels.manifest')}</Label>
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

              {tabs.length > 1 ? (
                <div className="inline-flex rounded-md border border-border overflow-hidden text-sm">
                  {tabs.map((tab) => (
                    <button
                      key={tab.key}
                      onClick={() => {
                        setField(tab.key);
                        setFilters([]);
                        setOffset(0);
                      }}
                      className={cn(
                        'px-3 py-1.5 transition-colors',
                        field === tab.key
                          ? 'bg-fg/10 text-fg font-medium'
                          : 'text-muted hover:bg-fg/5 hover:text-fg',
                      )}
                    >
                      {tab.label}
                    </button>
                  ))}
                </div>
              ) : null}

              <div className="space-y-2">
                <div className="flex items-center gap-2">
                  <Label className="text-xs text-muted">{t('labels.filters')}</Label>
                  <Button size="sm" variant="secondary" onClick={addFilter} disabled={columns.length === 0}>
                    {t('buttons.addFilter')}
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
                      {t('buttons.clearAll')}
                    </Button>
                  ) : null}
                </div>
                {filters.length === 0 ? (
                  <div className="text-xs text-muted">
                    {t('filters.emptyHint', { limit })}
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
                                {t(`ops.${o}`)}
                              </option>
                            ))}
                          </Select>
                          <Input
                            value={f.value}
                            onChange={(e) => updateFilter(f.uid, { value: e.target.value })}
                            placeholder={t('labels.value')}
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

      {hasView ? (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">
              {currentJob?.manifest_id} · {isScalar ? t('scalarTab') : currentSpec?.field}
            </CardTitle>
            <CardDescription>
              {isScalar ? (
                <span className="text-muted">{t('scalarSource')} · </span>
              ) : (
                <>
                  <code>{currentSpec?.table}</code> ·{' '}
                </>
              )}
              {t('labels.columns')}:{' '}
              {columns.map((c, i) => (
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
                <Loader2 className="size-4 animate-spin" /> {t('results.searching')}
              </div>
            ) : searchQ.error ? (
              <ErrorCard title={t('results.error')} error={searchQ.error} />
            ) : rows.length === 0 ? (
              <div className="text-sm text-muted py-4">
                {t('results.empty')}
              </div>
            ) : (
              <>
                <div className="text-xs text-muted mb-2">
                  {rows.length === limit
                    ? t('results.showingMore', {
                        from: offset + 1,
                        to: offset + rows.length,
                        limit,
                      })
                    : t('results.showing', {
                        from: offset + 1,
                        to: offset + rows.length,
                      })}
                </div>
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>{t('results.columns.pcId')}</TableHead>
                      <TableHead>{t('results.columns.collectedAt')}</TableHead>
                      {columns.map((c) => (
                        <TableHead key={c.field}>{c.field}</TableHead>
                      ))}
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {rows.map((row, i) => {
                      const pcId = formatCell(row.pc_id);
                      // Gemini #116 fix: derive a row key from the
                      // spec's primary_key tuple (= the SQL unique
                      // identity per (pc_id, job_id)) for explode
                      // results — index keys are a React anti-pattern
                      // under pagination / filter changes. Scalar
                      // results are one row per PC, so pc_id is already
                      // stable; offset+index is a defensive tiebreaker.
                      const rowKey = isScalar
                        ? `${pcId}-${offset + i}`
                        : [
                            pcId,
                            ...(currentSpec?.primary_key ?? []).map((k) => formatCell(row[k])),
                          ].join('|');
                      // Gemini #116 fix: pass row.collected_at
                      // through to fmtIsoLocal directly rather than
                      // round-tripping null through formatCell's dash.
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
                          {columns.map((c) => (
                            <TableCell key={c.field}>{formatCell(row[c.field])}</TableCell>
                          ))}
                        </TableRow>
                      );
                    })}
                  </TableBody>
                </Table>
                <div className="flex items-center gap-2 mt-3 text-xs">
                  <Label htmlFor="software-limit" className="text-muted">
                    {t('labels.pageSize')}
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
                    {t('buttons.prev')}
                  </Button>
                  <Button
                    size="sm"
                    variant="secondary"
                    onClick={() => setOffset(offset + limit)}
                    disabled={rows.length < limit}
                    title={
                      rows.length < limit
                        ? t('pagination.lastPage')
                        : t('pagination.nextPage')
                    }
                  >
                    {t('buttons.next')}
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
