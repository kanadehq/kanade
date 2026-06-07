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
import { PcPicker } from '@/components/PcPicker';
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
type SourcesResponse = { sources: string[] };

const SINCE_PRESETS: Array<{ value: string; ms: number | null }> = [
  { value: '1h',  ms: 60 * 60 * 1000 },
  { value: '24h', ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', ms: null },
];

// Issue #391: payload keys the collectors are known to emit,
// offered as <datalist> suggestions for the generic payload
// filter. Free text stays allowed — the suggestions are a memory
// aid, not a schema.
const PAYLOAD_KEY_SUGGESTIONS = [
  'user', 'logon_type', 'session_id', 'sid', 'standby', 'from', 'wake_source',
] as const;

// Issue #391: tri-state chip cycle — off → include → exclude → off.
type ChipState = 'off' | 'include' | 'exclude';

function chipState(value: string, inc: string[], exc: string[]): ChipState {
  if (inc.includes(value)) return 'include';
  if (exc.includes(value)) return 'exclude';
  return 'off';
}

function cycleChip(
  value: string,
  inc: string[],
  exc: string[],
  setInc: (v: string[]) => void,
  setExc: (v: string[]) => void,
) {
  if (inc.includes(value)) {
    setInc(inc.filter((v) => v !== value));
    setExc([...exc, value]);
  } else if (exc.includes(value)) {
    setExc(exc.filter((v) => v !== value));
  } else {
    setInc([...inc, value]);
  }
}

function splitCsv(s: string | null): string[] {
  // Mirror the backend's CSV hygiene (trim, drop blanks) and
  // dedupe, so a hand-edited / shared URL like `?kinds=logon,
  // logon , ,boot` hydrates the same chip state the server
  // filters by (CodeRabbit #394 minor).
  return s
    ? Array.from(new Set(s.split(',').map((v) => v.trim()).filter(Boolean)))
    : [];
}

function FilterChip({ label, state, onClick }: {
  label: string;
  state: ChipState;
  onClick: () => void;
}) {
  // State is conveyed by colour for sighted users; the aria-label
  // narrates it for screen readers (Gemini #394 medium). i18n keys
  // under `filters.chipStates.*`.
  const { t } = useTranslation('events');
  const cls =
    state === 'include'
      ? 'border-emerald-500/60 bg-emerald-500/15 text-emerald-300'
      : state === 'exclude'
        ? 'border-red-500/60 bg-red-500/10 text-red-400 line-through'
        : 'border-border text-muted hover:border-muted-foreground/50';
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={`${label}: ${t(`filters.chipStates.${state}`)}`}
      className={`rounded-full border px-2.5 py-0.5 text-xs cursor-pointer transition-colors ${cls}`}
    >
      {label}
    </button>
  );
}

// UAC split-token dedupe window (Issue #371). An interactive
// sign-in by an Administrators-group user writes TWO 4624s (full
// token + filtered token) microseconds apart, so a logon_type
// filtered view shows every human logon doubled. 2 s sits far
// above the observed pair distance (µs) and far below any genuine
// re-logon interval.
const DEDUPE_WINDOW_MS = 2_000;

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
    case 'unlock':
    case 'boot':
    case 'resume':
    case 'agent_started':
      return 'success';
    case 'logoff':
    case 'lock':
    case 'shutdown':
    case 'sleep':
      return 'default';
    case 'unexpected_shutdown':
      return 'danger';
    case 'diagnostic':
    case 'agent_self_update':
    case 'wake_detail':
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
  // Issue #391: include/exclude chip selections. Legacy single-value
  // params (`kind`, `source`, `logon_type`) from pre-#391 shared
  // URLs migrate into the new shape on first render so old links
  // keep meaning the same thing.
  const [kindsInc, setKindsInc] = useState<string[]>(() => {
    const v = splitCsv(search.get('kinds'));
    const legacy = search.get('kind');
    return v.length === 0 && legacy ? [legacy] : v;
  });
  const [kindsExc, setKindsExc] = useState<string[]>(() => splitCsv(search.get('kinds_ex')));
  const [sourcesInc, setSourcesInc] = useState<string[]>(() => {
    const v = splitCsv(search.get('sources'));
    const legacy = search.get('source');
    return v.length === 0 && legacy ? [legacy] : v;
  });
  const [sourcesExc, setSourcesExc] = useState<string[]>(() => splitCsv(search.get('sources_ex')));
  // Issue #391: generic payload key=value filter (subsumes the old
  // logon_type select — `logon_type` URLs map onto it).
  const [payloadKey, setPayloadKey] = useState(() =>
    search.get('pkey') ?? (search.get('logon_type') ? 'logon_type' : ''));
  const [payloadValue, setPayloadValue] = useState(() =>
    search.get('pval') ?? search.get('logon_type') ?? '');
  // Dedupe defaults ON; only the opt-out lands in the URL.
  const [dedupe, setDedupe] = useState(search.get('dedupe') !== '0');
  const [since, setSince] = useState(search.get('since') ?? '24h');
  const [limit, setLimit] = useState(Number(search.get('limit')) || 200);

  const sinceIso = useMemo(() => {
    const preset = SINCE_PRESETS.find((p) => p.value === since);
    if (!preset?.ms) return null;
    return new Date(Date.now() - preset.ms).toISOString();
  }, [since]);

  const dPcId         = useDebouncedValue(pcId,         FILTER_DEBOUNCE_MS);
  const dPayloadKey   = useDebouncedValue(payloadKey,   FILTER_DEBOUNCE_MS);
  const dPayloadValue = useDebouncedValue(payloadValue, FILTER_DEBOUNCE_MS);

  // Mirror filters into the URL so a timeline drill-down link is
  // shareable / reload-safe (same shape as Logs). Uses the debounced
  // values for the typed-text inputs so a keystroke doesn't write a
  // partial URL on every change (Gemini #252 HIGH). `replace: true`
  // keeps these writes out of the back/forward stack, so polluting
  // history is a non-issue — no separate URL→state sync needed.
  useEffect(() => {
    const next = new URLSearchParams();
    if (dPcId)   next.set('pc', dPcId);
    if (kindsInc.length)   next.set('kinds', kindsInc.join(','));
    if (kindsExc.length)   next.set('kinds_ex', kindsExc.join(','));
    if (sourcesInc.length) next.set('sources', sourcesInc.join(','));
    if (sourcesExc.length) next.set('sources_ex', sourcesExc.join(','));
    if (dPayloadKey)   next.set('pkey', dPayloadKey);
    if (dPayloadValue) next.set('pval', dPayloadValue);
    if (!dedupe) next.set('dedupe', '0');
    if (since && since !== '24h') next.set('since', since);
    if (limit && limit !== 200)   next.set('limit', String(limit));
    setSearch(next, { replace: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dPcId, kindsInc, kindsExc, sourcesInc, sourcesExc, dPayloadKey, dPayloadValue, dedupe, since, limit]);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (dPcId)   sp.set('pc_id', dPcId);
    if (kindsInc.length)   sp.set('kinds', kindsInc.join(','));
    if (kindsExc.length)   sp.set('kinds_ex', kindsExc.join(','));
    if (sourcesInc.length) sp.set('sources', sourcesInc.join(','));
    if (sourcesExc.length) sp.set('sources_ex', sourcesExc.join(','));
    // Both halves of the payload pair must be present — a key with
    // no value (or vice versa) constrains nothing server-side.
    if (dPayloadKey && dPayloadValue) {
      sp.set('payload_key', dPayloadKey);
      sp.set('payload_value', dPayloadValue);
    }
    if (sinceIso) sp.set('from', sinceIso);
    return sp.toString();
  }, [dPcId, kindsInc, kindsExc, sourcesInc, sourcesExc, dPayloadKey, dPayloadValue, sinceIso, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['obs_events', queryString],
    queryFn: () => apiFetch<ListResponse>(`/api/obs_events?${queryString}`),
  });

  // Chip vocabularies come from the backend's DISTINCT lists so the
  // operator picks from what actually exists in the data — no
  // hard-coded kind/source catalogue to maintain (Issue #391).
  const kindsQ = useQuery({
    queryKey: ['obs_events-kinds'],
    queryFn: () => apiFetch<KindsResponse>('/api/obs_events/kinds'),
    staleTime: 60_000,
  });
  const sourcesQ = useQuery({
    queryKey: ['obs_events-sources'],
    queryFn: () => apiFetch<SourcesResponse>('/api/obs_events/sources'),
    staleTime: 60_000,
  });

  // UAC split-token dedupe (Issue #371): collapse rows with the
  // same (pc, kind, payload.user) closer together than
  // DEDUPE_WINDOW_MS into the newest row of the group. View-only —
  // `obs_events` keeps both rows (faithful to the OS), and the
  // checkbox restores the raw view. Only active while the payload
  // filter targets logon_type (the forensic 4624 data set from
  // collect-winlog-logons-all / pre-#378 rows): that's the only
  // view where the split-token pairing reads as duplication rather
  // than data — Winlogon-sourced rows are 1:1 by construction.
  const dedupeApplicable = dPayloadKey === 'logon_type' && dPayloadValue !== '';
  const { visible, collapsed } = useMemo(() => {
    // Fallback array lives inside the memo so the `data === undefined`
    // render doesn't mint a fresh `[]` reference every pass, and the
    // dependency list stays exhaustive-deps-clean (Gemini #372 medium).
    const rows = data?.events ?? [];
    if (!dedupeApplicable || !dedupe) return { visible: rows, collapsed: 0 };
    const lastKept = new Map<string, number>();
    const visible: EventRow[] = [];
    let collapsed = 0;
    // Rows arrive `at DESC` from the API, so each group's newest
    // row is seen first and becomes the anchor its split-token
    // twin collapses into. The anchor only moves on a KEPT row, so
    // a slow drip of events spaced just inside the window can't
    // chain-collapse into one point.
    for (const e of rows) {
      const p = e.payload as { user?: unknown } | null;
      const user = typeof p?.user === 'string' ? p.user : '';
      const ts = Date.parse(e.at);
      if (Number.isNaN(ts)) {
        visible.push(e);
        continue;
      }
      const key = `${e.pc_id}|${e.kind}|${user}`;
      const anchor = lastKept.get(key);
      if (anchor !== undefined && anchor - ts < DEDUPE_WINDOW_MS) {
        collapsed++;
        continue;
      }
      lastKept.set(key, ts);
      visible.push(e);
    }
    return { visible, collapsed };
  }, [data, dedupeApplicable, dedupe]);

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <div className="flex items-baseline gap-2">
          {collapsed > 0 && (
            <span className="text-xs text-muted">
              {t('dedupeCollapsed', { count: collapsed })}
            </span>
          )}
          <Badge variant="violet">
            {isFetching && !isLoading
              ? t('countBadgeFetching', { count: visible.length })
              : t('countBadge', { count: visible.length })}
          </Badge>
        </div>
      </div>

      <Card>
        <CardContent className="space-y-3 p-4">
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-3">
            <div className="space-y-1">
              <Label htmlFor="ev-pc">{t('filters.pcId')}</Label>
              {/* filter mode keeps free text so the regex/substring backend filter still works */}
              <PcPicker
                mode="filter"
                id="ev-pc"
                placeholder={t('filters.placeholders.pcId')}
                value={pcId}
                onChange={setPcId}
              />
            </div>
            <div className="space-y-1">
              <Label>{t('filters.payload')}</Label>
              <div className="flex gap-1.5">
                <Input
                  id="ev-payload-key"
                  className="w-2/5"
                  list="ev-payload-keys"
                  placeholder={t('filters.placeholders.payloadKey')}
                  value={payloadKey}
                  onChange={(e) => setPayloadKey(e.target.value)}
                />
                <datalist id="ev-payload-keys">
                  {PAYLOAD_KEY_SUGGESTIONS.map((k) => <option key={k} value={k} />)}
                </datalist>
                <Input
                  id="ev-payload-value"
                  className="flex-1"
                  placeholder={t('filters.placeholders.payloadValue')}
                  value={payloadValue}
                  onChange={(e) => setPayloadValue(e.target.value)}
                />
              </div>
              {dedupeApplicable && (
                <div className="space-y-0.5 pt-1">
                  <label className="flex items-center gap-1.5 text-xs text-muted cursor-pointer">
                    <input
                      type="checkbox"
                      className="accent-violet-500"
                      checked={dedupe}
                      onChange={(e) => setDedupe(e.target.checked)}
                    />
                    {t('filters.dedupe')}
                  </label>
                  <p className="text-[11px] text-muted">{t('filters.forensicNote')}</p>
                </div>
              )}
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
          </div>
          {/* Issue #391: tri-state chips — click cycles include
              (green) → exclude (red, struck) → off. Vocabulary is
              the backend's DISTINCT list, so new kinds/sources show
              up here without SPA changes. */}
          <div className="space-y-1">
            <Label>{t('filters.kinds')} <span className="font-normal text-muted">{t('filters.chipHint')}</span></Label>
            <div className="flex flex-wrap gap-1.5">
              {(kindsQ.data?.kinds ?? []).map((k) => (
                <FilterChip
                  key={k}
                  label={k}
                  state={chipState(k, kindsInc, kindsExc)}
                  onClick={() => cycleChip(k, kindsInc, kindsExc, setKindsInc, setKindsExc)}
                />
              ))}
            </div>
          </div>
          <div className="space-y-1">
            <Label>{t('filters.sources')} <span className="font-normal text-muted">{t('filters.chipHint')}</span></Label>
            <div className="flex flex-wrap gap-1.5">
              {(sourcesQ.data?.sources ?? []).map((s) => (
                <FilterChip
                  key={s}
                  label={s}
                  state={chipState(s, sourcesInc, sourcesExc)}
                  onClick={() => cycleChip(s, sourcesInc, sourcesExc, setSourcesInc, setSourcesExc)}
                />
              ))}
            </div>
          </div>
        </CardContent>
      </Card>

      {isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />{t('loading')}
        </div>
      ) : error ? (
        <ErrorCard title={t('errorTitle')} error={error} />
      ) : visible.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>{t('empty.title')}</CardTitle></CardHeader>
          <CardContent className="text-muted">
            {t('empty.body')}
          </CardContent>
        </Card>
      ) : (
        <>
          <EventsTimeline events={visible} />
          <EventsHeatmap events={visible} />
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
            {visible.map((e) => (
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

// Time-unit constants shared by the heatmap's bucket math. At module
// scope so the bucket-size useMemo + the continuous-buckets useMemo
// + the human-label code all read from the same source (Gemini #268
// HIGH cleanup).
const HOUR_MS = 60 * 60 * 1000;
const DAY_MS = 24 * HOUR_MS;

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
    if (span <= 24 * HOUR_MS) {
      return {
        bucketMs: HOUR_MS,
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
    if (span <= 7 * DAY_MS) {
      return {
        bucketMs: 6 * HOUR_MS,
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
      bucketMs: DAY_MS,
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

  // (pc, bucket) → count + a CONTINUOUS bucket axis spanning lo..hi.
  //
  // Iterating from min to max in bucket-sized steps (instead of
  // collecting only the buckets that had events) is what makes the
  // X axis honest — idle stretches between events must show up as a
  // run of empty cells, otherwise non-consecutive bucket times
  // render side-by-side and recurrence patterns get misread (Gemini
  // #268 HIGH / CodeRabbit #268 Major).
  //
  // Date arithmetic via `setHours` / `setDate` (instead of raw
  // millisecond addition) keeps the iteration DST-safe — on a
  // forward-DST day a 24h span wall-clock-walks as 23 calendar
  // hours, and the setter handles that without dropping the last
  // bucket.
  const { buckets, counts, max } = useMemo(() => {
    const map = new Map<string, number>();
    let max = 0;
    let lo = Infinity;
    let hi = -Infinity;
    for (const ev of events) {
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      if (ts < lo) lo = ts;
      if (ts > hi) hi = ts;
      const b = alignBucket(ts);
      const key = `${ev.pc_id}|${b}`;
      const next = (map.get(key) ?? 0) + 1;
      map.set(key, next);
      if (next > max) max = next;
    }
    const buckets: number[] = [];
    if (isFinite(lo) && isFinite(hi)) {
      const end = alignBucket(hi);
      let current = alignBucket(lo);
      // Hard cap — a misconfigured filter (e.g. 30-day window with
      // a daily bucket and a clock skew) shouldn't blow up the DOM.
      // 400 cells × 50 PCs still renders cleanly; past that the
      // operator should narrow the window.
      while (current <= end && buckets.length < 400) {
        buckets.push(current);
        const d = new Date(current);
        if (bucketMs === HOUR_MS) {
          d.setHours(d.getHours() + 1);
        } else if (bucketMs === DAY_MS) {
          d.setDate(d.getDate() + 1);
        } else {
          // 6h bucket: step in hours so DST shifts realign on the
          // next 6h boundary instead of drifting an hour ahead.
          d.setHours(d.getHours() + bucketMs / HOUR_MS);
        }
        current = d.getTime();
      }
    }
    return { buckets, counts: map, max };
  }, [events, alignBucket, bucketMs]);

  if (buckets.length === 0 || pcs.length === 0) return null;

  // Empty cell base + busiest-cell colour. Same violet the
  // informational bucket of `kindColor` uses, so the page's chart
  // palette stays consistent.
  const cellBase = '#8b5cf6'; // violet-500

  // Human label for the auto-picked bucket size — surfaced next to
  // the title so the operator knows whether they're reading "events
  // per hour" or "events per day" without inferring from the column
  // labels.
  const bucketLabel = bucketMs === HOUR_MS ? '1h' : bucketMs === DAY_MS ? '1d' : `${bucketMs / HOUR_MS}h`;

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
