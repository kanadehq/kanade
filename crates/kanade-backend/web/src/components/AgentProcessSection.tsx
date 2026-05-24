import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Activity, Loader2, Power, RefreshCw } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Area,
  AreaChart,
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Select } from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type {
  ConfigScope,
  EffectiveConfigResponse,
  ProcessTimelineResponse,
  ProcessesResponse,
} from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

/** Durations the SPA exposes through the duration picker. Matches the
 *  values listed in `agent-detail.process.durations.*`. */
type DurationKey = '30m' | '1h' | '6h' | '24h';
const DURATION_SECONDS: Record<DurationKey, number> = {
  '30m': 30 * 60,
  '1h': 60 * 60,
  '6h': 6 * 60 * 60,
  '24h': 24 * 60 * 60,
};

/** Ticker for the "expires in N" countdown. 1 s feels alive without
 *  re-rendering more than the eye can follow. */
const COUNTDOWN_TICK_MS = 1000;
/** Cadence for polling the latest top-N snapshot. 5 s is brisk enough
 *  to feel live while the agent is publishing every ~60 s — most ticks
 *  will just re-render the same data, which Recharts/React no-op
 *  efficiently. */
const POLL_TICK_MS = 5000;

/** Chart range → server-side bucket size. Same convention as the
 *  host_perf chart on the parent page: pick `step` so each range
 *  lands in ~30-180 buckets, the band where Recharts and the eye
 *  both stay happy.
 *
 *  process_perf retention is 7 days so the longest range we expose
 *  here is 24h — anything beyond would routinely fall off the back
 *  of the table mid-zoom. */
type ChartRangeKey = '15m' | '30m' | '1h' | '6h' | '24h';
const CHART_RANGE_TO_STEP: Record<
  ChartRangeKey,
  { fromSecondsAgo: number; step: string; stepSecs: number }
> = {
  '15m': { fromSecondsAgo: 15 * 60, step: '30s', stepSecs: 30 },
  '30m': { fromSecondsAgo: 30 * 60, step: '1m', stepSecs: 60 },
  '1h': { fromSecondsAgo: 60 * 60, step: '1m', stepSecs: 60 },
  '6h': { fromSecondsAgo: 6 * 60 * 60, step: '5m', stepSecs: 300 },
  '24h': { fromSecondsAgo: 24 * 60 * 60, step: '15m', stepSecs: 900 },
};

/** Metric the chart projects. Matches the backend's `TimelineMetric`
 *  alias accepting `cpu`/`rss`/`disk_read`/`disk_written`. */
type ChartMetric = 'cpu' | 'rss' | 'disk_read' | 'disk_written';
const CHART_METRICS: ChartMetric[] = ['cpu', 'rss', 'disk_read', 'disk_written'];

/** Top-N selector. 5 is the default; 10 lets an operator who wants to
 *  see more tail processes step up without uncapping the legend. */
const TOP_N_OPTIONS = [5, 10] as const;
type TopNOption = (typeof TOP_N_OPTIONS)[number];

/** Right-edge advance: align with the finest bucket we offer. The
 *  memo below floors "now" to a bucket boundary so React Query only
 *  refetches when the floored value actually crosses a bucket. */
const CHART_RIGHT_EDGE_TICK_MS = 30_000;

/** Stable palette for the stacked series. Pinned by index so the
 *  same "rank N in window" name keeps the same colour across renders.
 *  Last entry is reserved for the `other` collapsed series. */
const SERIES_COLOURS = [
  '#8b5cf6', // violet
  '#06b6d4', // cyan
  '#10b981', // emerald
  '#f59e0b', // amber
  '#ef4444', // red
  '#3b82f6', // blue
  '#ec4899', // pink
  '#84cc16', // lime
  '#a855f7', // purple
  '#14b8a6', // teal
];
const OTHER_COLOUR = '#6b7280'; // slate-500 — neutral grey for the tail

function fmtBytes(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v) || v < 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  let n = v;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n < 10 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}
function fmtBytesPerSec(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v) || v < 0) return '—';
  return `${fmtBytes(v)}/s`;
}
function fmtRemaining(ms: number): string {
  if (ms <= 0) return '0s';
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function fmtPct(v: unknown): string {
  return typeof v === 'number' && Number.isFinite(v) ? `${v.toFixed(1)}%` : '—';
}
function fmtAxisTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
function tooltipLabel(label: unknown): string {
  return typeof label === 'string' ? fmtIsoLocal(label) : '';
}

export function AgentProcessSection({ pcId }: { pcId: string }) {
  const { t } = useTranslation('agent-detail');
  const qc = useQueryClient();
  const [duration, setDuration] = useState<DurationKey>('30m');
  const [now, setNow] = useState(() => Date.now());

  // Live countdown — re-renders once per second so the "expires in"
  // display stays current without burning a per-component
  // requestAnimationFrame loop.
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), COUNTDOWN_TICK_MS);
    return () => clearInterval(id);
  }, []);

  const effectiveQ = useQuery({
    queryKey: ['agent-effective-config', pcId],
    queryFn: () =>
      apiFetch<EffectiveConfigResponse>(
        `/api/agents/${encodeURIComponent(pcId)}/effective_config`,
      ),
    refetchInterval: POLL_TICK_MS,
    enabled: !!pcId,
  });

  // The PUT endpoint expects a complete ConfigScope (it replaces the
  // pcs.<pc_id> row wholesale). Read the existing scope first so we
  // can merge our process_perf_* changes without clobbering
  // unrelated fields the operator set with `kanade config set`.
  const pcScopeQ = useQuery({
    queryKey: ['pc-config', pcId],
    queryFn: () =>
      apiFetch<ConfigScope>(`/api/pcs/${encodeURIComponent(pcId)}/config`),
    enabled: !!pcId,
  });

  const processesQ = useQuery({
    queryKey: ['processes', pcId],
    queryFn: () =>
      apiFetch<ProcessesResponse>(`/api/agents/${encodeURIComponent(pcId)}/processes`),
    refetchInterval: POLL_TICK_MS,
    enabled: !!pcId,
  });

  const toggleMut = useMutation({
    mutationFn: (next: ConfigScope) =>
      apiFetch<ConfigScope>(`/api/pcs/${encodeURIComponent(pcId)}/config`, {
        method: 'PUT',
        body: JSON.stringify(next),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['agent-effective-config', pcId] });
      qc.invalidateQueries({ queryKey: ['pc-config', pcId] });
    },
  });

  const effective = effectiveQ.data?.effective;
  const enabledFlag = effective?.process_perf_enabled ?? false;
  const expiresAt = effective?.process_perf_expires_at ?? null;
  const expiresMs = expiresAt ? new Date(expiresAt).getTime() : null;
  const isActive = enabledFlag && (expiresMs === null || expiresMs > now);
  const isExpired = enabledFlag && expiresMs !== null && expiresMs <= now;
  const remainingMs = expiresMs && isActive ? expiresMs - now : 0;

  const setEnabled = (enable: boolean) => {
    const base: ConfigScope = pcScopeQ.data ?? {};
    if (enable) {
      const newExpiry = new Date(
        Date.now() + DURATION_SECONDS[duration] * 1000,
      ).toISOString();
      toggleMut.mutate({
        ...base,
        process_perf_enabled: true,
        process_perf_expires_at: newExpiry,
      });
    } else {
      // Disable: clear the flag. Leaving `expires_at` set is fine —
      // the agent checks both, and the SPA shows OFF as long as the
      // flag itself is false. Avoids one redundant PUT field.
      toggleMut.mutate({
        ...base,
        process_perf_enabled: false,
      });
    }
  };

  const statusBadge = (() => {
    if (isActive)
      return <Badge variant="violet">{t('process.statusActive')}</Badge>;
    if (isExpired)
      return <Badge variant="amber">{t('process.statusExpired')}</Badge>;
    return <Badge variant="violet">{t('process.statusOff')}</Badge>;
  })();

  const subline = (() => {
    if (isActive) {
      return (
        <span className="text-xs text-muted">
          {t('process.remaining', { remaining: fmtRemaining(remainingMs) })}
        </span>
      );
    }
    if (isExpired) {
      return (
        <span className="text-xs text-muted">
          {t('process.expiredAt', { at: fmtIsoLocal(expiresAt) })}
        </span>
      );
    }
    return null;
  })();

  const latestAt = processesQ.data?.latest_at ?? null;
  const rows = processesQ.data?.processes ?? [];

  return (
    <Card>
      <CardHeader className="flex flex-row items-start justify-between gap-3">
        <div className="space-y-1">
          <div className="flex items-center gap-2">
            <CardTitle className="text-base">{t('process.title')}</CardTitle>
            {statusBadge}
            {subline}
          </div>
          <p className="text-xs text-muted">{t('process.intro')}</p>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          {!isActive ? (
            <>
              <span className="text-xs text-muted">
                {t('process.durationLabel')}
              </span>
              <Select
                value={duration}
                onChange={(e) => setDuration(e.target.value as DurationKey)}
                className="w-32"
              >
                {(Object.keys(DURATION_SECONDS) as DurationKey[]).map((k) => (
                  <option key={k} value={k}>
                    {t(`process.durations.${k}`)}
                  </option>
                ))}
              </Select>
              <Button
                size="sm"
                onClick={() => setEnabled(true)}
                disabled={toggleMut.isPending || pcScopeQ.isLoading}
              >
                <Power className="size-3.5" />
                {t('process.enable', {
                  duration: t(`process.durations.${duration}`),
                })}
              </Button>
            </>
          ) : (
            <>
              <Button
                variant="secondary"
                size="sm"
                onClick={() => setEnabled(true)}
                disabled={toggleMut.isPending}
                title={t('process.extend')}
              >
                <RefreshCw className="size-3.5" />
                {t('process.extend')}
              </Button>
              <Button
                variant="danger"
                size="sm"
                onClick={() => setEnabled(false)}
                disabled={toggleMut.isPending}
              >
                <Power className="size-3.5" />
                {t('process.disable')}
              </Button>
            </>
          )}
        </div>
      </CardHeader>
      <CardContent>
        {processesQ.isLoading && (
          <div className="flex items-center gap-2 text-muted text-sm">
            <Loader2 className="size-4 animate-spin" />
            {t('process.loading')}
          </div>
        )}
        {processesQ.error && (
          <ErrorCard title={t('process.errorTitle')} error={processesQ.error} />
        )}
        {!processesQ.isLoading && !processesQ.error && rows.length === 0 && (
          <p className="text-muted text-sm">
            {isActive ? t('process.emptyOn') : t('process.emptyOff')}
          </p>
        )}
        {rows.length > 0 && <ProcessTimelineChart pcId={pcId} />}
        {rows.length > 0 && (
          <div className="space-y-2 mt-6">
            <div className="text-xs text-muted flex items-center gap-2">
              <Activity className="size-3" />
              {t('process.latestAt', { at: fmtIsoLocal(latestAt) })}
            </div>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="text-right">
                    {t('process.columns.pid')}
                  </TableHead>
                  <TableHead>{t('process.columns.name')}</TableHead>
                  <TableHead className="text-right">
                    {t('process.columns.cpu')}
                  </TableHead>
                  <TableHead className="text-right">
                    {t('process.columns.rss')}
                  </TableHead>
                  <TableHead className="text-right">
                    {t('process.columns.diskRead')}
                  </TableHead>
                  <TableHead className="text-right">
                    {t('process.columns.diskWritten')}
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((p) => (
                  <TableRow key={p.pid}>
                    <TableCell className="text-right text-muted text-xs">
                      {p.pid}
                    </TableCell>
                    <TableCell className="text-xs">
                      <code>{p.name}</code>
                    </TableCell>
                    <TableCell className="text-right text-xs">
                      {p.cpu_pct.toFixed(1)}%
                    </TableCell>
                    <TableCell className="text-right text-xs">
                      {fmtBytes(p.rss_bytes)}
                    </TableCell>
                    <TableCell className="text-right text-muted text-xs">
                      {fmtBytesPerSec(p.disk_read_bytes_per_sec)}
                    </TableCell>
                    <TableCell className="text-right text-muted text-xs">
                      {fmtBytesPerSec(p.disk_written_bytes_per_sec)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function ProcessTimelineChart({ pcId }: { pcId: string }) {
  const { t } = useTranslation('agent-detail');
  const [range, setRange] = useState<ChartRangeKey>('30m');
  const [metric, setMetric] = useState<ChartMetric>('cpu');
  const [topN, setTopN] = useState<TopNOption>(5);

  // Floor "now" to the active step so the React Query key only
  // changes on a bucket crossing — same pattern as the host_perf
  // chart on the parent page.
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), CHART_RIGHT_EDGE_TICK_MS);
    return () => clearInterval(id);
  }, []);
  const { fromIso, toIso, stepStr } = useMemo(() => {
    const { fromSecondsAgo, step, stepSecs } = CHART_RANGE_TO_STEP[range];
    const stepMs = stepSecs * 1000;
    const toMs = Math.floor(Date.now() / stepMs) * stepMs;
    const fromMs = toMs - fromSecondsAgo * 1000;
    return {
      fromIso: new Date(fromMs).toISOString(),
      toIso: new Date(toMs).toISOString(),
      stepStr: step,
    };
    // `tick` is the heartbeat dep — see the host_perf chart memo for
    // the same pattern.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [range, tick]);

  const timelineQ = useQuery({
    queryKey: ['processes-timeline', pcId, metric, topN, fromIso, toIso, stepStr],
    queryFn: () =>
      apiFetch<ProcessTimelineResponse>(
        `/api/agents/${encodeURIComponent(pcId)}/processes/timeline` +
          `?metric=${encodeURIComponent(metric)}` +
          `&from=${encodeURIComponent(fromIso)}` +
          `&to=${encodeURIComponent(toIso)}` +
          `&step=${encodeURIComponent(stepStr)}` +
          `&top=${topN}`,
      ),
    enabled: !!pcId,
  });

  // Recharts wants a flat row per x-tick: { at, "chrome.exe": 42, ... }.
  // Treat name-absent-from-this-bucket as 0 so the stack reads as the
  // total — that's why the backend doc says "missing means 0 when
  // stacking".
  const chartRows = useMemo(() => {
    const names = timelineQ.data?.names ?? [];
    const points = timelineQ.data?.points ?? [];
    return points.map((p) => {
      const row: Record<string, number | string> = { at: p.at };
      for (const n of names) row[n] = p.values[n] ?? 0;
      return row;
    });
  }, [timelineQ.data]);

  const isBytesMetric = metric === 'rss';
  const isRateMetric = metric === 'disk_read' || metric === 'disk_written';
  const yTickFormatter = (v: number) => {
    if (isBytesMetric) return fmtBytes(v);
    if (isRateMetric) return fmtBytesPerSec(v);
    return `${Math.round(v)}%`;
  };
  const tooltipFormatter = (value: unknown): string => {
    if (typeof value !== 'number' || !Number.isFinite(value)) return '—';
    if (isBytesMetric) return fmtBytes(value);
    if (isRateMetric) return fmtBytesPerSec(value);
    return fmtPct(value);
  };

  const names = timelineQ.data?.names ?? [];

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="space-y-0.5">
          <h3 className="text-sm font-medium">{t('process.chart.title')}</h3>
          <p className="text-xs text-muted">
            {t('process.chart.intro', { top: topN })}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs text-muted">
            {t('process.chart.metricLabel')}
          </span>
          <Select
            value={metric}
            onChange={(e) => setMetric(e.target.value as ChartMetric)}
            className="w-40"
          >
            {CHART_METRICS.map((m) => (
              <option key={m} value={m}>
                {t(`process.chart.metrics.${m}`)}
              </option>
            ))}
          </Select>
          <span className="text-xs text-muted">
            {t('process.chart.rangeLabel')}
          </span>
          <Select
            value={range}
            onChange={(e) => setRange(e.target.value as ChartRangeKey)}
            className="w-28"
          >
            {(Object.keys(CHART_RANGE_TO_STEP) as ChartRangeKey[]).map((k) => (
              <option key={k} value={k}>
                {t(`process.chart.ranges.${k}`)}
              </option>
            ))}
          </Select>
          <span className="text-xs text-muted">{t('process.chart.topLabel')}</span>
          <Select
            value={String(topN)}
            onChange={(e) => setTopN(Number(e.target.value) as TopNOption)}
            className="w-20"
          >
            {TOP_N_OPTIONS.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </Select>
        </div>
      </div>
      <div className="text-xs text-muted">
        {t('process.chart.stepNote', { step: stepStr })}
      </div>
      {timelineQ.isLoading && (
        <div className="flex items-center gap-2 text-muted text-sm">
          <Loader2 className="size-4 animate-spin" />
          {t('process.chart.loading')}
        </div>
      )}
      {timelineQ.error && (
        <ErrorCard
          title={t('process.chart.errorTitle')}
          error={timelineQ.error}
        />
      )}
      {!timelineQ.isLoading && !timelineQ.error && chartRows.length === 0 && (
        <p className="text-muted text-sm">{t('process.chart.empty')}</p>
      )}
      {chartRows.length > 0 && (
        <ResponsiveContainer width="100%" height={280}>
          <AreaChart data={chartRows} margin={{ top: 8, right: 12, bottom: 4, left: 0 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
            <XAxis dataKey="at" tickFormatter={fmtAxisTime} fontSize={11} />
            <YAxis tickFormatter={yTickFormatter} fontSize={11} width={isBytesMetric || isRateMetric ? 80 : 50} />
            <Tooltip labelFormatter={tooltipLabel} formatter={tooltipFormatter} />
            <Legend />
            {names.map((n, idx) => {
              const isOther = n === 'other';
              const colour = isOther
                ? OTHER_COLOUR
                : SERIES_COLOURS[idx % SERIES_COLOURS.length];
              const displayName = isOther ? t('process.chart.other') : n;
              return (
                <Area
                  key={n}
                  type="monotone"
                  dataKey={n}
                  name={displayName}
                  stackId="processes"
                  stroke={colour}
                  fill={colour}
                  fillOpacity={0.45}
                  isAnimationActive={false}
                />
              );
            })}
          </AreaChart>
        </ResponsiveContainer>
      )}
    </div>
  );
}
