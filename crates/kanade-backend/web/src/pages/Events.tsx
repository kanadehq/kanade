import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { Fragment, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import {
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Scatter,
  ScatterChart,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

type EventRow = {
  id: number;
  pc_id: string;
  at: string;
  kind: string;
  source: string;
  event_record_id: string | null;
  payload: unknown;
};

type ListResponse = { events: EventRow[] };
type KindsResponse = { kinds: string[] };

const SINCE_PRESETS: Array<{ value: string; ms: number | null }> = [
  { value: '1h',  ms: 60 * 60 * 1000 },
  { value: '24h', ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', ms: null },
];

const FILTER_DEBOUNCE_MS = 300;

function useDebouncedValue<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), delay);
    return () => clearTimeout(t);
  }, [value, delay]);
  return debounced;
}

// Map the kind vocabulary spelt out in #246 onto the four existing
// badge palettes so an operator skimming the timeline can chunk
// lifecycle (success/danger) vs informational (violet) vs neutral
// (default) without reading every cell.
function kindVariant(kind: string): 'success' | 'amber' | 'danger' | 'violet' | 'default' {
  switch (kind) {
    case 'logon':
    case 'boot':
    case 'resume':
    case 'agent_started':
      return 'success';
    case 'logoff':
    case 'shutdown':
    case 'sleep':
      return 'default';
    case 'unexpected_shutdown':
      return 'danger';
    case 'diagnostic':
    case 'agent_self_update':
      return 'violet';
    default:
      return 'amber';
  }
}

// Hex equivalents of the Tailwind palette used in `kindVariant` —
// Recharts needs concrete fills, not class names. Kept aligned with
// the existing chart colours in AgentDetail.tsx so the operator's
// eye carries between pages.
const KIND_COLORS: Record<string, string> = {
  success: '#10b981', // emerald-500
  amber:   '#f59e0b', // amber-500
  danger:  '#ef4444', // red-500
  violet:  '#8b5cf6', // violet-500
  default: '#94a3b8', // slate-400
};
function kindColor(kind: string): string {
  return KIND_COLORS[kindVariant(kind)] ?? KIND_COLORS.default;
}

export function Events() {
  const { t } = useTranslation('events');
  const [search, setSearch] = useSearchParams();
  const [pcId, setPcId] = useState(search.get('pc') ?? '');
  const [kind, setKind] = useState(search.get('kind') ?? '');
  const [source, setSource] = useState(search.get('source') ?? '');
  const [since, setSince] = useState(search.get('since') ?? '24h');
  const [limit, setLimit] = useState(Number(search.get('limit')) || 200);

  const sinceIso = useMemo(() => {
    const preset = SINCE_PRESETS.find((p) => p.value === since);
    if (!preset?.ms) return null;
    return new Date(Date.now() - preset.ms).toISOString();
  }, [since]);

  const dPcId   = useDebouncedValue(pcId,   FILTER_DEBOUNCE_MS);
  const dSource = useDebouncedValue(source, FILTER_DEBOUNCE_MS);

  // Mirror filters into the URL so a timeline drill-down link is
  // shareable / reload-safe (same shape as Logs). Uses the debounced
  // values for the typed-text inputs so a keystroke doesn't write a
  // partial URL on every change (Gemini #252 HIGH). `replace: true`
  // keeps these writes out of the back/forward stack, so polluting
  // history is a non-issue — no separate URL→state sync needed.
  useEffect(() => {
    const next = new URLSearchParams();
    if (dPcId)   next.set('pc', dPcId);
    if (kind)    next.set('kind', kind);
    if (dSource) next.set('source', dSource);
    if (since && since !== '24h') next.set('since', since);
    if (limit && limit !== 200)   next.set('limit', String(limit));
    setSearch(next, { replace: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dPcId, kind, dSource, since, limit]);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (dPcId)   sp.set('pc_id', dPcId);
    if (kind)    sp.set('kind', kind);
    if (dSource) sp.set('source', dSource);
    if (sinceIso) sp.set('from', sinceIso);
    return sp.toString();
  }, [dPcId, kind, dSource, sinceIso, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['obs_events', queryString],
    queryFn: () => apiFetch<ListResponse>(`/api/obs_events?${queryString}`),
  });

  // Populate the kind <select> from the backend's distinct list so the
  // operator can pick what's actually in the table instead of guessing
  // strings from the spec. Falls back to "(any)" when empty.
  const kindsQ = useQuery({
    queryKey: ['obs_events-kinds'],
    queryFn: () => apiFetch<KindsResponse>('/api/obs_events/kinds'),
    staleTime: 60_000,
  });

  const rows = data?.events ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <Badge variant="violet">
          {isFetching && !isLoading
            ? t('countBadgeFetching', { count: rows.length })
            : t('countBadge', { count: rows.length })}
        </Badge>
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="ev-pc">{t('filters.pcId')}</Label>
            <Input
              id="ev-pc"
              placeholder={t('filters.placeholders.pcId')}
              value={pcId}
              onChange={(e) => setPcId(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-kind">{t('filters.kind')}</Label>
            <Select id="ev-kind" value={kind} onChange={(e) => setKind(e.target.value)}>
              <option value="">{t('filters.kindOptions.any')}</option>
              {(kindsQ.data?.kinds ?? []).map((k) => (
                <option key={k} value={k}>{k}</option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-source">{t('filters.source')}</Label>
            <Input
              id="ev-source"
              placeholder={t('filters.placeholders.source')}
              value={source}
              onChange={(e) => setSource(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-since">{t('filters.since')}</Label>
            <Select id="ev-since" value={since} onChange={(e) => setSince(e.target.value)}>
              {SINCE_PRESETS.map((p) => (
                <option key={p.value} value={p.value}>
                  {t(`filters.sincePresets.${p.value}`)}
                </option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-limit">{t('filters.limit')}</Label>
            <Select
              id="ev-limit"
              value={String(limit)}
              onChange={(e) => setLimit(Number(e.target.value))}
            >
              <option value="50">50</option>
              <option value="200">200</option>
              <option value="1000">1000</option>
              <option value="5000">5000</option>
            </Select>
          </div>
        </CardContent>
      </Card>

      {isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />{t('loading')}
        </div>
      ) : error ? (
        <ErrorCard title={t('errorTitle')} error={error} />
      ) : rows.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>{t('empty.title')}</CardTitle></CardHeader>
          <CardContent className="text-muted">
            {t('empty.body')}
          </CardContent>
        </Card>
      ) : (
        <>
          <EventsTimeline events={rows} />
          <EventsHeatmap events={rows} />
          <Table>
          <TableHeader>
            <TableRow>
              <TableHead>{t('columns.when')}</TableHead>
              <TableHead>{t('columns.pcId')}</TableHead>
              <TableHead>{t('columns.kind')}</TableHead>
              <TableHead>{t('columns.source')}</TableHead>
              <TableHead>{t('columns.recordId')}</TableHead>
              <TableHead>{t('columns.payload')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((e) => (
              <TableRow key={e.id}>
                <TableCell className="text-muted text-xs">{fmtIsoLocal(e.at)}</TableCell>
                <TableCell><code className="text-xs">{e.pc_id}</code></TableCell>
                <TableCell>
                  <Badge variant={kindVariant(e.kind)}>{e.kind}</Badge>
                </TableCell>
                <TableCell><code className="text-xs">{e.source}</code></TableCell>
                <TableCell>
                  {e.event_record_id
                    ? <code className="text-xs">{e.event_record_id}</code>
                    : <span className="text-muted text-xs">—</span>}
                </TableCell>
                <TableCell>
                  <details>
                    <summary className="cursor-pointer text-muted text-xs">{t('payload.show')}</summary>
                    <pre className="text-xs whitespace-pre-wrap break-words mt-2 bg-muted/5 p-2 rounded max-h-96 overflow-y-auto">
                      {JSON.stringify(e.payload, null, 2)}
                    </pre>
                  </details>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        </>
      )}
    </div>
  );
}

/**
 * Per-PC timeline scatter — X axis is time, Y axis is PC name,
 * point colour encodes `kind`. One Scatter series per kind so the
 * legend doubles as a colour key and operators can pick a kind to
 * highlight just by hovering its legend entry.
 *
 * Sits above the existing detail table on the Events page; both
 * read from the same `rows` array so toggling filters at the top
 * narrows the chart and table together.
 */
function EventsTimeline({ events }: { events: EventRow[] }) {
  const { t } = useTranslation('events');

  // Stable PC list (sorted) drives the Y axis category domain. Doing
  // this once via useMemo so re-renders for tooltip hover don't
  // re-sort the list and confuse Recharts' axis caching.
  const pcs = useMemo(() => {
    const set = new Set(events.map((e) => e.pc_id));
    return Array.from(set).sort();
  }, [events]);

  // Group points by kind so we can render one Scatter series per
  // kind (auto-coloured legend). Each point carries the original
  // event so the tooltip can render full context.
  const byKind = useMemo(() => {
    const out: Record<string, Array<{ ts: number; pc: string; ev: EventRow }>> = {};
    for (const ev of events) {
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      (out[ev.kind] ??= []).push({ ts, pc: ev.pc_id, ev });
    }
    return out;
  }, [events]);

  // Height scales with PC count so a fleet-wide view doesn't squash
  // every row; floor at 200 keeps the single-PC case readable.
  const chartHeight = Math.max(200, 48 + pcs.length * 36);

  // Pre-compute the time window so the X axis is consistent across
  // re-renders. Recharts can auto-domain, but `type="number"`
  // requires an explicit domain to render the axis labels right.
  const [tMin, tMax] = useMemo(() => {
    let lo = Infinity;
    let hi = -Infinity;
    for (const ev of events) {
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      if (ts < lo) lo = ts;
      if (ts > hi) hi = ts;
    }
    if (!isFinite(lo) || !isFinite(hi)) return [Date.now() - 60_000, Date.now()];
    // Pad 2% on each side so points don't hug the axis.
    const pad = Math.max(60_000, (hi - lo) * 0.02);
    return [lo - pad, hi + pad];
  }, [events]);

  // Decide X-axis tick format: short HH:mm when the window fits in a
  // day; switch to MM/DD HH:mm for multi-day ranges so the operator
  // can tell which Tuesday is which. useMemo keeps the function ref
  // stable across re-renders so Recharts doesn't trip its animation
  // diff on tooltip hover (Gemini #267 MEDIUM).
  const spanMs = tMax - tMin;
  const fmtTick = useMemo(() => {
    return (v: number) => {
      const d = new Date(v);
      if (spanMs > 24 * 60 * 60 * 1000) {
        return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`;
      }
      return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`;
    };
  }, [spanMs]);

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">{t('chart.title')}</CardTitle>
      </CardHeader>
      <CardContent className="pt-0">
        <ResponsiveContainer width="100%" height={chartHeight}>
          <ScatterChart margin={{ top: 8, right: 24, bottom: 16, left: 0 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
            <XAxis
              type="number"
              dataKey="ts"
              domain={[tMin, tMax]}
              tickFormatter={fmtTick}
              tick={{ fontSize: 11 }}
              stroke="currentColor"
              className="text-muted"
            />
            <YAxis
              type="category"
              dataKey="pc"
              // Explicit domain pins the row order even when a kind
              // series doesn't include every PC.
              domain={pcs}
              allowDuplicatedCategory={false}
              tick={{ fontSize: 11 }}
              width={120}
              stroke="currentColor"
              className="text-muted"
            />
            <Tooltip
              cursor={{ strokeDasharray: '3 3' }}
              content={({ active, payload }) => {
                if (!active || !payload?.length) return null;
                // Recharts can surface synthetic hover frames (e.g.
                // during cursor transitions) where `payload[0].payload`
                // exists but the embedded `ev` doesn't yet — bail out
                // before destructuring to avoid a tooltip-render crash
                // (Gemini #267 MEDIUM).
                const p = payload[0].payload as { ev?: EventRow } | undefined;
                if (!p?.ev) return null;
                const { ev } = p;
                return (
                  <div className="bg-card border border-border rounded px-2 py-1.5 text-xs shadow-md">
                    <div className="font-semibold">{ev.kind}</div>
                    <div className="text-muted">{fmtIsoLocal(ev.at)}</div>
                    <div><code>{ev.pc_id}</code> · <code>{ev.source}</code></div>
                  </div>
                );
              }}
            />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            {Object.entries(byKind).map(([k, pts]) => (
              <Scatter key={k} name={k} data={pts} fill={kindColor(k)} />
            ))}
          </ScatterChart>
        </ResponsiveContainer>
      </CardContent>
    </Card>
  );
}

/**
 * PC × time-bucket heatmap. Where the scatter above shows each
 * individual event as a point, the heatmap aggregates into buckets
 * so longer windows (a week, a month) collapse into a readable
 * grid of "this PC, this hour: 12 events" cells. Useful for
 * spotting recurring patterns the scatter loses in cloud-density.
 *
 * Bucket size auto-adapts to the rendered window:
 *   ≤ 24h → 1 hour buckets  (max 24 columns)
 *   ≤ 7d  → 6 hour buckets  (max 28 columns)
 *   else  → 1 day buckets   (matches the operator's "since: 30d" pick)
 *
 * Colour intensity uses the same violet hue as `chart.kindColor`'s
 * informational bucket, scaled by `count / max(count)` so the
 * busiest cell is fully saturated. CSS grid + per-cell opacity keeps
 * the impl dependency-free (no heatmap library) — Recharts doesn't
 * ship a heatmap component.
 */
function EventsHeatmap({ events }: { events: EventRow[] }) {
  const { t } = useTranslation('events');

  // PC list — same shape as the scatter so the two charts read in the
  // same row order when stacked.
  const pcs = useMemo(() => {
    const set = new Set(events.map((e) => e.pc_id));
    return Array.from(set).sort();
  }, [events]);

  // Determine the bucket size + the bucket alignment fn. Aligning to
  // wall-clock boundaries (top-of-hour, midnight) keeps the columns
  // labelled with round numbers operators can match against their
  // own log of "what was I doing at 14:00".
  const { bucketMs, alignBucket, fmtBucket } = useMemo(() => {
    let lo = Infinity;
    let hi = -Infinity;
    for (const ev of events) {
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      if (ts < lo) lo = ts;
      if (ts > hi) hi = ts;
    }
    const span = isFinite(hi - lo) ? hi - lo : 0;
    const HOUR = 60 * 60 * 1000;
    const DAY = 24 * HOUR;
    if (span <= 24 * HOUR) {
      return {
        bucketMs: HOUR,
        alignBucket: (ts: number) => {
          const d = new Date(ts);
          d.setMinutes(0, 0, 0);
          return d.getTime();
        },
        fmtBucket: (ts: number) => {
          const d = new Date(ts);
          return `${d.getHours().toString().padStart(2, '0')}`;
        },
      };
    }
    if (span <= 7 * DAY) {
      return {
        bucketMs: 6 * HOUR,
        alignBucket: (ts: number) => {
          const d = new Date(ts);
          d.setHours(Math.floor(d.getHours() / 6) * 6, 0, 0, 0);
          return d.getTime();
        },
        fmtBucket: (ts: number) => {
          const d = new Date(ts);
          return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours().toString().padStart(2, '0')}`;
        },
      };
    }
    return {
      bucketMs: DAY,
      alignBucket: (ts: number) => {
        const d = new Date(ts);
        d.setHours(0, 0, 0, 0);
        return d.getTime();
      },
      fmtBucket: (ts: number) => {
        const d = new Date(ts);
        return `${d.getMonth() + 1}/${d.getDate()}`;
      },
    };
  }, [events]);

  // (pc, bucket) → count. Also tracks the bucket key set + max for
  // colour normalisation.
  const { buckets, counts, max } = useMemo(() => {
    const set = new Set<number>();
    const map = new Map<string, number>();
    let max = 0;
    for (const ev of events) {
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      const b = alignBucket(ts);
      set.add(b);
      const key = `${ev.pc_id}|${b}`;
      const next = (map.get(key) ?? 0) + 1;
      map.set(key, next);
      if (next > max) max = next;
    }
    const buckets = Array.from(set).sort((a, b) => a - b);
    return { buckets, counts: map, max };
  }, [events, alignBucket]);

  if (buckets.length === 0 || pcs.length === 0) return null;

  // Empty cell base + busiest-cell colour. Same violet the
  // informational bucket of `kindColor` uses, so the page's chart
  // palette stays consistent.
  const cellBase = '#8b5cf6'; // violet-500

  // Human label for the auto-picked bucket size — surfaced next to
  // the title so the operator knows whether they're reading "events
  // per hour" or "events per day" without inferring from the column
  // labels.
  const HOUR = 60 * 60 * 1000;
  const DAY = 24 * HOUR;
  const bucketLabel = bucketMs === HOUR ? '1h' : bucketMs === DAY ? '1d' : `${bucketMs / HOUR}h`;

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">
          {t('heatmap.title')}
          <span className="ml-2 text-xs text-muted font-normal">
            {t('heatmap.bucketHint', { bucket: bucketLabel })}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="pt-0 overflow-x-auto">
        <div
          style={{
            display: 'grid',
            // First column = PC name label, then one column per bucket.
            // `auto` for the label so it sizes to the widest pc_id,
            // `minmax(18px, 1fr)` for cells so they stay visible on a
            // wide window but tile cleanly on a narrow one.
            gridTemplateColumns: `auto repeat(${buckets.length}, minmax(18px, 1fr))`,
            gap: '2px',
            fontSize: '10px',
          }}
        >
          {/* header row: empty corner + bucket labels */}
          <div />
          {buckets.map((b) => (
            <div key={b} className="text-muted text-center leading-none pt-1" title={fmtIsoLocal(new Date(b).toISOString())}>
              {fmtBucket(b)}
            </div>
          ))}
          {/* per-PC rows: pc_id label + cells. Fragment shorthand
              `<>` doesn't accept a `key`, so use the explicit
              React.Fragment form to silence the reconciler warning. */}
          {pcs.map((pc) => (
            <Fragment key={pc}>
              <div className="text-muted pr-2 self-center">
                <code className="text-[10px]">{pc}</code>
              </div>
              {buckets.map((b) => {
                const c = counts.get(`${pc}|${b}`) ?? 0;
                // Linear scale (count/max) saturates fast on long-tail
                // event distributions, but for the 0..~20 range typical
                // of a fleet's daily logon/sleep tally it reads well
                // enough that the log-scale alternative isn't worth
                // the explanation overhead.
                const alpha = c === 0 ? 0 : 0.15 + 0.85 * (c / max);
                return (
                  <div
                    key={`${pc}-${b}`}
                    className="rounded-sm h-5"
                    style={{
                      backgroundColor: c === 0
                        ? 'rgba(148, 163, 184, 0.08)' // slate-400 very light = "no events"
                        : `rgba(139, 92, 246, ${alpha})`,
                    }}
                    title={`${pc} · ${fmtIsoLocal(new Date(b).toISOString())}: ${t('heatmap.cellTooltip', { count: c })}`}
                  />
                );
              })}
            </Fragment>
          ))}
        </div>
        {/* Legend strip: 0 → max, gradient swatch */}
        <div className="mt-3 flex items-center gap-2 text-xs text-muted">
          <span>0</span>
          <div
            className="h-3 w-32 rounded-sm"
            style={{
              background: `linear-gradient(to right, rgba(148,163,184,0.08), ${cellBase})`,
            }}
          />
          <span>{max}</span>
        </div>
      </CardContent>
    </Card>
  );
}
