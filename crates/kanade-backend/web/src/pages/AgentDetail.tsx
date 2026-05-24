import { useQuery } from '@tanstack/react-query';
import { ArrowLeft, Loader2 } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useParams } from 'react-router-dom';
import {
  CartesianGrid,
  Legend,
  Line,
  LineChart,
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
import { apiFetch } from '@/lib/api';
import type { AgentRow, PerfResponse } from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

type RangeKey = '1h' | '6h' | '24h' | '7d' | '30d';

/** Range-to-server-bucket map. Step is picked so the chart fits in
 *  roughly 60-200 points regardless of zoom — Recharts handles that
 *  range smoothly and the eye can't usefully resolve more on a typical
 *  detail-page chart width.
 *
 *  `stepSecs` mirrors `step` numerically so the page-level tick logic
 *  can floor "now" to a bucket boundary without re-parsing the
 *  humantime string. */
const RANGE_TO_STEP: Record<RangeKey, { fromSecondsAgo: number; step: string; stepSecs: number }> = {
  '1h': { fromSecondsAgo: 60 * 60, step: '1m', stepSecs: 60 },
  '6h': { fromSecondsAgo: 6 * 60 * 60, step: '5m', stepSecs: 300 },
  '24h': { fromSecondsAgo: 24 * 60 * 60, step: '15m', stepSecs: 900 },
  '7d': { fromSecondsAgo: 7 * 24 * 60 * 60, step: '1h', stepSecs: 3600 },
  '30d': { fromSecondsAgo: 30 * 24 * 60 * 60, step: '4h', stepSecs: 14400 },
};

/** Heartbeat that drives the chart's right-edge advance. We tick
 *  once per minute (the finest bucket size in use); the memo below
 *  floors "now" to the active step's boundary, so within a single
 *  bucket the floored value stays put and React Query doesn't
 *  refire. On a bucket crossing the floored value jumps and the
 *  query refetches, sliding the chart forward in discrete steps. */
const RIGHT_EDGE_TICK_MS = 60_000;

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

function fmtAxisTime(iso: string): string {
  // Compact axis tick — drop the seconds + year for axis readability.
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

// Recharts' Tooltip signatures pass the label/value as the broad
// `ReactNode` / `ValueType | undefined`; narrow back to the shapes
// our formatters expect.
function tooltipLabel(label: unknown): string {
  return typeof label === 'string' ? fmtIsoLocal(label) : '';
}
function tooltipPct(value: unknown): string {
  return typeof value === 'number' && Number.isFinite(value) ? `${value.toFixed(1)}%` : '—';
}
function tooltipBytes(value: unknown): string {
  return typeof value === 'number' ? fmtBytes(value) : '—';
}
function tooltipBytesPerSec(value: unknown): string {
  return typeof value === 'number' ? fmtBytesPerSec(value) : '—';
}

export function AgentDetail() {
  const { t } = useTranslation('agent-detail');
  const { pcId = '' } = useParams<{ pcId: string }>();
  const [range, setRange] = useState<RangeKey>('1h');

  const agentQ = useQuery({
    queryKey: ['agent', pcId],
    queryFn: () => apiFetch<AgentRow>(`/api/agents/${encodeURIComponent(pcId)}`),
    enabled: !!pcId,
  });

  // Once-per-minute tick that the memo depends on. Plain state +
  // setInterval keeps it simple; the tick value itself is unused —
  // we only need a dep that changes to invalidate the memo.
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), RIGHT_EDGE_TICK_MS);
    return () => clearInterval(id);
  }, []);

  // Recompute from/to each tick so the right edge advances to "now"
  // while the user keeps the page open — Gemini Code Assist #192
  // caught the earlier `[range]`-only deps pinning the chart to the
  // open-time snapshot. "now" is floored to the active step's bucket
  // boundary, so the iso strings (= React Query keys) only change on
  // a bucket crossing, NOT on every minute tick when the floored
  // value happens to stay put — that keeps refetch traffic aligned
  // with the actual data resolution.
  const { fromIso, toIso, stepStr } = useMemo(() => {
    const { fromSecondsAgo, step, stepSecs } = RANGE_TO_STEP[range];
    const stepMs = stepSecs * 1000;
    const toMs = Math.floor(Date.now() / stepMs) * stepMs;
    const fromMs = toMs - fromSecondsAgo * 1000;
    return {
      fromIso: new Date(fromMs).toISOString(),
      toIso: new Date(toMs).toISOString(),
      stepStr: step,
    };
    // `tick` is intentionally in the deps even though its value
    // isn't read — see comment block above.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [range, tick]);

  const perfQ = useQuery({
    queryKey: ['agent-perf', pcId, range, fromIso, toIso],
    queryFn: () =>
      apiFetch<PerfResponse>(
        `/api/agents/${encodeURIComponent(pcId)}/perf?from=${encodeURIComponent(
          fromIso,
        )}&to=${encodeURIComponent(toIso)}&step=${encodeURIComponent(stepStr)}`,
      ),
    enabled: !!pcId,
  });

  if (agentQ.isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />
        {t('loading')}
      </div>
    );
  }
  if (agentQ.error) {
    return <ErrorCard title={t('errorTitle')} error={agentQ.error} />;
  }
  if (!agentQ.data) {
    return <ErrorCard title={t('errorTitle')} error={new Error(t('notFound'))} />;
  }
  const agent = agentQ.data;
  const points = perfQ.data?.points ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <div className="flex items-center gap-3">
          <Button variant="ghost" size="sm" asChild>
            <Link to="/agents">
              <ArrowLeft className="size-4" />
              {t('backToList')}
            </Link>
          </Button>
          <h2 className="text-xl">
            <code className="text-base">{agent.pc_id}</code>
          </h2>
          {agent.os_family && <Badge variant="violet">{agent.os_family}</Badge>}
        </div>
      </div>

      <Card>
        <CardContent className="grid grid-cols-2 md:grid-cols-5 gap-4 p-4 text-sm">
          <Meta label={t('meta.pcId')} value={<code className="text-xs">{agent.pc_id}</code>} />
          <Meta label={t('meta.hostname')} value={agent.hostname ?? '—'} />
          <Meta label={t('meta.os')} value={agent.os_family ?? '—'} />
          <Meta label={t('meta.agent')} value={agent.agent_version ?? '—'} />
          <Meta label={t('meta.lastHeartbeat')} value={fmtIsoLocal(agent.last_heartbeat)} />
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-3">
          <div className="space-y-1">
            <CardTitle className="text-base">{t('perf.title')}</CardTitle>
            <p className="text-xs text-muted">{t('perf.intro', { interval: '60s' })}</p>
          </div>
          <div className="flex items-center gap-2 shrink-0">
            <span className="text-xs text-muted">{t('perf.rangeLabel')}</span>
            <Select
              value={range}
              onChange={(e) => setRange(e.target.value as RangeKey)}
              className="w-32"
            >
              {(Object.keys(RANGE_TO_STEP) as RangeKey[]).map((k) => (
                <option key={k} value={k}>
                  {t(`perf.ranges.${k}`)}
                </option>
              ))}
            </Select>
          </div>
        </CardHeader>
        <CardContent className="space-y-6">
          {perfQ.isLoading && (
            <div className="flex items-center gap-2 text-muted text-sm">
              <Loader2 className="size-4 animate-spin" />
              {t('perf.loading')}
            </div>
          )}
          {perfQ.error && <ErrorCard title={t('perf.errorTitle')} error={perfQ.error} />}
          {!perfQ.isLoading && !perfQ.error && points.length === 0 && (
            <p className="text-muted text-sm">{t('perf.empty')}</p>
          )}
          {points.length > 0 && (
            <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
              <ChartCard title={t('perf.charts.cpu')} stepNote={t('perf.stepNote', { step: stepStr })}>
                <ResponsiveContainer width="100%" height={220}>
                  <LineChart data={points} margin={{ top: 8, right: 12, bottom: 4, left: 0 }}>
                    <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
                    <XAxis dataKey="at" tickFormatter={fmtAxisTime} fontSize={11} />
                    <YAxis
                      domain={[0, 100]}
                      tickFormatter={(v) => `${v}%`}
                      fontSize={11}
                      width={45}
                    />
                    <Tooltip labelFormatter={tooltipLabel} formatter={tooltipPct} />
                    <Legend />
                    <Line
                      type="monotone"
                      dataKey="cpu_pct"
                      name={t('perf.series.cpu_pct')}
                      stroke="#8b5cf6"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </ChartCard>

              <ChartCard title={t('perf.charts.memory')} stepNote={t('perf.stepNote', { step: stepStr })}>
                <ResponsiveContainer width="100%" height={220}>
                  <LineChart data={points} margin={{ top: 8, right: 12, bottom: 4, left: 0 }}>
                    <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
                    <XAxis dataKey="at" tickFormatter={fmtAxisTime} fontSize={11} />
                    <YAxis tickFormatter={fmtBytes} fontSize={11} width={70} />
                    <Tooltip labelFormatter={tooltipLabel} formatter={tooltipBytes} />
                    <Legend />
                    <Line
                      type="monotone"
                      dataKey="mem_used_bytes"
                      name={t('perf.series.mem_used')}
                      stroke="#06b6d4"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                    <Line
                      type="monotone"
                      dataKey="mem_total_bytes"
                      name={t('perf.series.mem_total')}
                      stroke="#06b6d4"
                      strokeDasharray="4 4"
                      strokeWidth={1}
                      dot={false}
                      isAnimationActive={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </ChartCard>

              <ChartCard title={t('perf.charts.disk')} stepNote={t('perf.stepNote', { step: stepStr })}>
                <ResponsiveContainer width="100%" height={220}>
                  <LineChart data={points} margin={{ top: 8, right: 12, bottom: 4, left: 0 }}>
                    <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
                    <XAxis dataKey="at" tickFormatter={fmtAxisTime} fontSize={11} />
                    <YAxis tickFormatter={fmtBytesPerSec} fontSize={11} width={80} />
                    <Tooltip labelFormatter={tooltipLabel} formatter={tooltipBytesPerSec} />
                    <Legend />
                    <Line
                      type="monotone"
                      dataKey="disk_read_bytes_per_sec"
                      name={t('perf.series.disk_read')}
                      stroke="#10b981"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                    <Line
                      type="monotone"
                      dataKey="disk_written_bytes_per_sec"
                      name={t('perf.series.disk_written')}
                      stroke="#f59e0b"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </ChartCard>

              <ChartCard title={t('perf.charts.network')} stepNote={t('perf.stepNote', { step: stepStr })}>
                <ResponsiveContainer width="100%" height={220}>
                  <LineChart data={points} margin={{ top: 8, right: 12, bottom: 4, left: 0 }}>
                    <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
                    <XAxis dataKey="at" tickFormatter={fmtAxisTime} fontSize={11} />
                    <YAxis tickFormatter={fmtBytesPerSec} fontSize={11} width={80} />
                    <Tooltip labelFormatter={tooltipLabel} formatter={tooltipBytesPerSec} />
                    <Legend />
                    <Line
                      type="monotone"
                      dataKey="net_rx_bytes_per_sec"
                      name={t('perf.series.net_rx')}
                      stroke="#3b82f6"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                    <Line
                      type="monotone"
                      dataKey="net_tx_bytes_per_sec"
                      name={t('perf.series.net_tx')}
                      stroke="#ef4444"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </ChartCard>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

function Meta({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="space-y-1">
      <div className="text-xs text-muted">{label}</div>
      <div>{value}</div>
    </div>
  );
}

function ChartCard({
  title,
  stepNote,
  children,
}: {
  title: string;
  stepNote: string;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-1">
      <div className="flex items-baseline justify-between">
        <h3 className="text-sm font-medium">{title}</h3>
        <span className="text-xs text-muted">{stepNote}</span>
      </div>
      {children}
    </div>
  );
}
