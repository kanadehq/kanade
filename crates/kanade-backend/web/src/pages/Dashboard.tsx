import { useQuery } from '@tanstack/react-query';
import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  Clock,
  Cpu,
  Gauge,
  HelpCircle,
  LineChart as LineChartIcon,
  MemoryStick,
  Search,
  Server,
  Users,
  Wifi,
  XCircle,
} from 'lucide-react';
import { Trans, useTranslation } from 'react-i18next';
import { Link } from 'react-router-dom';
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

import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';

import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { ApiError, apiFetch } from '@/lib/api';
import { cn, isAgentOnline } from '@/lib/utils';
import type {
  ActiveInvestigationsResponse,
  AgentRow,
  FleetPerfResponse,
  JetstreamSnapshot,
  TopPerfResponse,
} from '@/lib/types';

/** v0.37 / agent perf: per-job duration aggregates from
 *  /api/health/scan_durations. One row per job_id that finished
 *  at least once in the window. Renders as the "Scan duration"
 *  card below — slowest first so the operator's first glance
 *  catches the probe that's hurting. */
type ScanDurationStats = {
  job_id: string;
  count: number;
  min_ms: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
  max_ms: number;
  mean_ms: number;
};

function fmtMs(ms: number): string {
  if (ms < 1) return '<1 ms';
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

type FleetHealth = {
  status: 'ok' | 'unknown' | 'degraded';
  agents: { known: number; active: number; stale: number };
  jetstream: { all_ok: boolean; healthy: number; total: number; missing: string[] };
  recent_results: { window_hours: number; total: number; failed: number };
  observed_at: string;
};

type ResultRow = {
  request_id: string;
  pc_id: string;
  exit_code: number;
  started_at: string | null;
  finished_at: string | null;
};

type AuditRow = {
  id: number;
  actor: string;
  action: string;
  target: string | null;
  occurred_at: string;
};

function fmtRelative(iso: string | null): string {
  if (!iso) return 'never';
  const ts = new Date(iso).getTime();
  if (isNaN(ts)) return iso;
  const delta = Date.now() - ts;
  if (delta < 0) return 'in the future';
  const sec = Math.floor(delta / 1000);
  if (sec < 60) return `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  return `${Math.floor(hr / 24)}d ago`;
}

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

// #493: the sliding-window `from` is computed at fetch time (inside
// queryFn — the HistoryPane pattern) so the queryKey stays stable.
// The previous inline `new Date(Date.now() - 24h)` in the component
// body minted a new ms-precision key on every render — each fetch
// completion re-rendered, which minted another key, which fetched
// again: a self-sustaining loop against /api/perf/fleet (a
// whole-fleet aggregate), with refetchInterval never applying
// because no key lived long enough. Refetches are driven by
// refetchInterval and re-anchor to "now" each time. Module-level so
// the helper isn't re-created per render.
const FLEET_PERF_WINDOW_HOURS = 24;
function fleetPerfUrl(metric: string): string {
  const from = new Date(
    Date.now() - FLEET_PERF_WINDOW_HOURS * 60 * 60 * 1000,
  ).toISOString();
  return `/api/perf/fleet?metric=${metric}&agg=avg&from=${encodeURIComponent(from)}&step=15m`;
}

function fmtAxisTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

// Recharts label/value formatters narrowed from the broad
// `ReactNode` / `ValueType | undefined` shapes Tooltip passes in.
function tooltipLabel(label: unknown): string {
  return typeof label === 'string' ? fmtRelative(label) : '';
}
function tooltipPct(value: unknown): string {
  return typeof value === 'number' && Number.isFinite(value)
    ? `${value.toFixed(1)}%`
    : '—';
}
function tooltipBytes(value: unknown): string {
  return typeof value === 'number' ? fmtBytes(value) : '—';
}

// React Query refetch cadence shared by every panel on this page,
// and the same value the i18n strings interpolate as `{{seconds}}` —
// keeps the displayed "auto-refresh: Ns" copy in lockstep with the
// actual polling interval. Bump in one place.
const REFRESH_INTERVAL_MS = 30_000;
const REFRESH_INTERVAL_SECONDS = REFRESH_INTERVAL_MS / 1000;

function StatBlock({
  label,
  value,
  hint,
  tone = 'default',
}: {
  label: string;
  value: number | string;
  hint?: string;
  tone?: 'default' | 'success' | 'danger';
}) {
  return (
    <div className="flex flex-col">
      <span className="text-xs uppercase tracking-wide text-muted">{label}</span>
      <span
        className={cn(
          'text-3xl font-bold tabular-nums leading-tight',
          tone === 'success' && 'text-success',
          tone === 'danger' && 'text-danger',
        )}
      >
        {value}
      </span>
      {hint && <span className="text-xs text-muted mt-0.5">{hint}</span>}
    </div>
  );
}

export function Dashboard() {
  const { t } = useTranslation('dashboard');
  const agentsQ = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const jsQ = useQuery({
    queryKey: ['jetstream-status'],
    queryFn: () => apiFetch<JetstreamSnapshot>('/api/jetstream/status'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const resultsQ = useQuery({
    queryKey: ['results-recent'],
    queryFn: () => apiFetch<ResultRow[]>('/api/results?limit=8'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const auditQ = useQuery({
    queryKey: ['audit-recent'],
    queryFn: () => apiFetch<AuditRow[]>('/api/audit?limit=8'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  // /api/health/fleet returns 503 when degraded so we have to peel
  // the body off the ApiError to render the rollup either way.
  const healthQ = useQuery({
    queryKey: ['health-fleet'],
    queryFn: async () => {
      try {
        return await apiFetch<FleetHealth>('/api/health/fleet');
      } catch (e) {
        if (e instanceof ApiError && e.status === 503 && e.body) {
          return JSON.parse(e.body) as FleetHealth;
        }
        throw e;
      }
    },
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const scanDurQ = useQuery({
    queryKey: ['scan-durations'],
    queryFn: () => apiFetch<ScanDurationStats[]>('/api/health/scan_durations'),
    refetchInterval: 60_000,
  });

  // v0.41 / Phase 3: fleet aggregate cards. The 24h CPU+Mem
  // sparkline shares a single query that fetches each metric
  // separately (sparkline cards each hit `/api/perf/fleet` with a
  // different metric, both 15 min bucket → 96 points over 24h).
  // The sliding-window `from` is computed inside queryFn via the
  // module-level fleetPerfUrl helper — see its doc comment (#493).
  const fleetCpuQ = useQuery({
    queryKey: ['fleet-perf', 'cpu'],
    queryFn: () => apiFetch<FleetPerfResponse>(fleetPerfUrl('cpu_pct')),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const fleetMemQ = useQuery({
    queryKey: ['fleet-perf', 'mem'],
    queryFn: () => apiFetch<FleetPerfResponse>(fleetPerfUrl('mem_used_bytes')),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const topCpuQ = useQuery({
    queryKey: ['top-perf', 'cpu'],
    queryFn: () =>
      apiFetch<TopPerfResponse>('/api/perf/top?metric=cpu_pct&window=5m&limit=5'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const topMemQ = useQuery({
    queryKey: ['top-perf', 'mem'],
    queryFn: () =>
      apiFetch<TopPerfResponse>(
        '/api/perf/top?metric=mem_used_bytes&window=5m&limit=5',
      ),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const activeInvQ = useQuery({
    queryKey: ['active-investigations'],
    queryFn: () =>
      apiFetch<ActiveInvestigationsResponse>('/api/perf/active-investigations'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });

  const agents = agentsQ.data ?? [];
  // Same 2-min heartbeat threshold the Agents page uses for its
  // online/offline badge — keeps "active / known" here in lockstep
  // with the per-row status there, and the tile deep-links across.
  const active = agents.filter((a) => isAgentOnline(a.last_heartbeat)).length;

  const js = jsQ.data;
  const jsRows = js ? [...js.streams, ...js.kv_buckets, ...js.object_stores] : [];
  const jsOk = jsRows.filter((r) => r.exists).length;
  const jsAllOk = js !== undefined && jsRows.every((r) => r.exists);

  const recentFail = (resultsQ.data ?? []).filter((r) => r.exit_code !== 0).length;
  const recentTotal = (resultsQ.data ?? []).length;

  const health = healthQ.data;
  const healthMeta = health
    ? (
        {
          ok:       { icon: CheckCircle2,  tone: 'success' as const, statusKey: 'ok' as const },
          unknown:  { icon: HelpCircle,    tone: 'default' as const, statusKey: 'unknown' as const },
          degraded: { icon: AlertTriangle, tone: 'danger'  as const, statusKey: 'degraded' as const },
        }
      )[health.status]
    : null;

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-2xl">{t('title')}</h2>
        <span className="text-xs text-muted">
          {t('autoRefresh', { seconds: REFRESH_INTERVAL_SECONDS })}
        </span>
      </div>

      {health && healthMeta && (
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <healthMeta.icon
                className={cn(
                  'size-5',
                  healthMeta.tone === 'success' && 'text-success',
                  healthMeta.tone === 'danger' && 'text-danger',
                  healthMeta.tone === 'default' && 'text-muted',
                )}
              />
              {t('fleetHealth.title')}
              <Badge
                variant={
                  healthMeta.tone === 'success' ? 'success'
                  : healthMeta.tone === 'danger' ? 'danger'
                  : 'amber'
                }
              >
                {t(`fleetHealth.status.${healthMeta.statusKey}.label`)}
              </Badge>
            </CardTitle>
            <CardDescription>
              <Trans
                ns="dashboard"
                i18nKey="fleetHealth.description"
                values={{ hint: t(`fleetHealth.status.${healthMeta.statusKey}.hint`) }}
                components={{ code: <code /> }}
              />
            </CardDescription>
          </CardHeader>
          <CardContent className="grid grid-cols-2 md:grid-cols-4 gap-6">
            {/* Deep-link to the Agents list, pre-filtered to the
                offline hosts when any are stale — answers "the N that
                aren't connected, which are they?" in one click. */}
            <Link
              to={health.agents.stale > 0 ? '/agents?status=offline' : '/agents'}
              className="rounded-md -m-1 p-1 transition-colors hover:bg-muted/10"
              title={t('fleetHealth.stats.agents.linkTitle')}
            >
              <StatBlock
                label={t('fleetHealth.stats.agents.label')}
                value={`${health.agents.active} / ${health.agents.known}`}
                hint={t('fleetHealth.stats.agents.hint')}
                tone={health.agents.stale > 0 ? 'danger' : 'default'}
              />
            </Link>
            <Link
              to={health.agents.stale > 0 ? '/agents?status=offline' : '/agents'}
              className="rounded-md -m-1 p-1 transition-colors hover:bg-muted/10"
              title={t('fleetHealth.stats.staleAgents.linkTitle')}
            >
              <StatBlock
                label={t('fleetHealth.stats.staleAgents.label')}
                value={health.agents.stale}
                tone={health.agents.stale > 0 ? 'danger' : 'success'}
                hint={t('fleetHealth.stats.staleAgents.hint')}
              />
            </Link>
            <StatBlock
              label={t('fleetHealth.stats.jetstream.label')}
              value={`${health.jetstream.healthy} / ${health.jetstream.total}`}
              tone={health.jetstream.all_ok ? 'success' : 'danger'}
              hint={
                health.jetstream.missing.length > 0
                  ? t('fleetHealth.stats.jetstream.hintMissing', {
                      names: health.jetstream.missing.join(', '),
                    })
                  : t('fleetHealth.stats.jetstream.hintHealthy')
              }
            />
            {/* Deep-link into the Activity list, pre-filtered to the
                failed runs when any failed — one click from "3 / 40"
                to the three that didn't exit clean. The 24h failures
                window matches Activity's default `since`, so the
                filtered list mirrors this count. */}
            <Link
              to={
                health.recent_results.failed > 0
                  ? '/activity?status=failure'
                  : '/activity'
              }
              className="rounded-md -m-1 p-1 transition-colors hover:bg-muted/10"
              title={t('fleetHealth.stats.failures.linkTitle')}
            >
              <StatBlock
                label={t('fleetHealth.stats.failures.label', {
                  hours: health.recent_results.window_hours,
                })}
                value={`${health.recent_results.failed} / ${health.recent_results.total}`}
                tone={health.recent_results.failed > 0 ? 'danger' : 'default'}
                hint={t('fleetHealth.stats.failures.hint')}
              />
            </Link>
          </CardContent>
        </Card>
      )}

      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Users className="size-5 text-violet" />
              {t('fleet.title')}
            </CardTitle>
            <CardDescription>
              <Trans
                ns="dashboard"
                i18nKey="fleet.description"
                components={{ code: <code /> }}
              />
            </CardDescription>
          </CardHeader>
          <CardContent className="flex gap-8 items-end">
            <Link
              to="/agents"
              className="rounded-md -m-1 p-1 transition-colors hover:bg-muted/10"
              title={t('fleet.stats.linkTitle')}
            >
              <StatBlock label={t('fleet.stats.known')} value={agents.length} />
            </Link>
            <Link
              to={active < agents.length ? '/agents?status=offline' : '/agents'}
              className="rounded-md -m-1 p-1 transition-colors hover:bg-muted/10"
              title={t('fleet.stats.linkTitle')}
            >
              <StatBlock
                label={t('fleet.stats.activeLabel')}
                value={active}
                tone={active === agents.length && agents.length > 0 ? 'success' : 'default'}
                hint={t('fleet.stats.activeHint')}
              />
            </Link>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Server className="size-5 text-violet" />
              {t('jetstream.title')}
            </CardTitle>
            <CardDescription>
              <Trans
                ns="dashboard"
                i18nKey="jetstream.description"
                components={{ code: <code /> }}
              />
            </CardDescription>
          </CardHeader>
          <CardContent className="flex gap-8 items-end">
            <StatBlock
              label={t('jetstream.resources.label')}
              value={js ? `${jsOk} / ${jsRows.length}` : '—'}
              tone={jsAllOk ? 'success' : js ? 'danger' : 'default'}
              hint={t('jetstream.resources.hint')}
            />
            {js && (
              <div className="flex flex-col gap-1">
                {jsAllOk ? (
                  <Badge variant="success">{t('jetstream.allHealthy')}</Badge>
                ) : (
                  <Badge variant="danger">
                    {t('jetstream.missing', { count: jsRows.length - jsOk })}
                  </Badge>
                )}
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Activity className="size-5 text-violet" />
              {t('recentResults.title')}
            </CardTitle>
            <CardDescription>
              {recentFail > 0
                ? t('recentResults.descriptionFailed', { count: recentTotal, failed: recentFail })
                : t('recentResults.descriptionAllGreen', { count: recentTotal })}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            {(resultsQ.data ?? []).map((r) => (
              <div key={r.request_id} className="flex items-center gap-3 text-sm">
                {r.exit_code === 0 ? (
                  <CheckCircle2 className="size-4 text-success shrink-0" />
                ) : (
                  <XCircle className="size-4 text-danger shrink-0" />
                )}
                <code className="text-xs">{r.request_id.slice(0, 8)}</code>
                <span className="text-muted">·</span>
                <code className="text-xs">{r.pc_id}</code>
                <span className="text-muted text-xs ml-auto">{fmtRelative(r.finished_at)}</span>
              </div>
            ))}
            {(resultsQ.data ?? []).length === 0 && (
              <div className="text-muted text-sm">{t('recentResults.empty')}</div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Clock className="size-5 text-violet" />
              {t('recentActivity.title')}
            </CardTitle>
            <CardDescription>{t('recentActivity.description')}</CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            {(auditQ.data ?? []).map((e) => (
              <div key={e.id} className="flex items-center gap-3 text-sm">
                <Badge
                  variant={e.actor === 'scheduler' ? 'violet' : 'amber'}
                  className="shrink-0"
                >
                  {e.actor}
                </Badge>
                <code className="text-xs">{e.action}</code>
                {e.target && <span className="text-muted text-xs truncate">{e.target}</span>}
                <span className="text-muted text-xs ml-auto whitespace-nowrap">
                  {fmtRelative(e.occurred_at)}
                </span>
              </div>
            ))}
            {(auditQ.data ?? []).length === 0 && (
              <div className="text-muted text-sm">{t('recentActivity.empty')}</div>
            )}
          </CardContent>
        </Card>
      </div>

      {/* v0.41 / Phase 3: fleet-wide perf aggregates. The sparkline
          card plus the two Top-N tables give the operator a
          three-second read on "anything pegging the fleet RIGHT
          now". The "investigations" card is the safety net for
          process_perf toggles left on by mistake. */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <LineChartIcon className="size-5 text-violet" />
            {t('fleetPerf.title')}
          </CardTitle>
          <CardDescription>
            <Trans
              ns="dashboard"
              i18nKey="fleetPerf.description"
              components={{ code: <code /> }}
            />
          </CardDescription>
        </CardHeader>
        <CardContent>
          {(fleetCpuQ.data?.points.length ?? 0) === 0 &&
          (fleetMemQ.data?.points.length ?? 0) === 0 ? (
            <div className="text-muted text-sm">{t('fleetPerf.empty')}</div>
          ) : (
            <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
              <div className="space-y-1">
                <div className="text-xs uppercase tracking-wide text-muted">
                  {t('fleetPerf.series.cpu')}
                </div>
                <ResponsiveContainer width="100%" height={180}>
                  <LineChart
                    data={fleetCpuQ.data?.points ?? []}
                    margin={{ top: 8, right: 12, bottom: 4, left: 0 }}
                  >
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
                      dataKey="value"
                      name={t('fleetPerf.series.cpu')}
                      stroke="#8b5cf6"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </div>
              <div className="space-y-1">
                <div className="text-xs uppercase tracking-wide text-muted">
                  {t('fleetPerf.series.mem')}
                </div>
                <ResponsiveContainer width="100%" height={180}>
                  <LineChart
                    data={fleetMemQ.data?.points ?? []}
                    margin={{ top: 8, right: 12, bottom: 4, left: 0 }}
                  >
                    <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
                    <XAxis dataKey="at" tickFormatter={fmtAxisTime} fontSize={11} />
                    <YAxis tickFormatter={fmtBytes} fontSize={11} width={70} />
                    <Tooltip labelFormatter={tooltipLabel} formatter={tooltipBytes} />
                    <Legend />
                    <Line
                      type="monotone"
                      dataKey="value"
                      name={t('fleetPerf.series.mem')}
                      stroke="#06b6d4"
                      strokeWidth={2}
                      dot={false}
                      isAnimationActive={false}
                    />
                  </LineChart>
                </ResponsiveContainer>
              </div>
            </div>
          )}
        </CardContent>
      </Card>

      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Cpu className="size-5 text-violet" />
              {t('topCpu.title')}
            </CardTitle>
            <CardDescription>
              <Trans
                ns="dashboard"
                i18nKey="topCpu.description"
                components={{ code: <code /> }}
              />
            </CardDescription>
          </CardHeader>
          <CardContent>
            {(topCpuQ.data?.rows ?? []).length === 0 ? (
              <div className="text-muted text-sm">{t('topCpu.empty')}</div>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('topTable.pcId')}</TableHead>
                    <TableHead>{t('topTable.hostname')}</TableHead>
                    <TableHead className="text-right">{t('topTable.value')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {(topCpuQ.data?.rows ?? []).map((r) => (
                    <TableRow key={r.pc_id}>
                      <TableCell>
                        <Link
                          to={`/agents/${encodeURIComponent(r.pc_id)}`}
                          className="hover:underline"
                        >
                          <code className="text-xs">{r.pc_id}</code>
                        </Link>
                      </TableCell>
                      <TableCell className="text-muted text-xs">
                        {r.hostname ?? '—'}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {r.value.toFixed(1)}%
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <MemoryStick className="size-5 text-violet" />
              {t('topMem.title')}
            </CardTitle>
            <CardDescription>
              <Trans
                ns="dashboard"
                i18nKey="topMem.description"
                components={{ code: <code /> }}
              />
            </CardDescription>
          </CardHeader>
          <CardContent>
            {(topMemQ.data?.rows ?? []).length === 0 ? (
              <div className="text-muted text-sm">{t('topMem.empty')}</div>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('topTable.pcId')}</TableHead>
                    <TableHead>{t('topTable.hostname')}</TableHead>
                    <TableHead className="text-right">{t('topTable.value')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {(topMemQ.data?.rows ?? []).map((r) => (
                    <TableRow key={r.pc_id}>
                      <TableCell>
                        <Link
                          to={`/agents/${encodeURIComponent(r.pc_id)}`}
                          className="hover:underline"
                        >
                          <code className="text-xs">{r.pc_id}</code>
                        </Link>
                      </TableCell>
                      <TableCell className="text-muted text-xs">
                        {r.hostname ?? '—'}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {fmtBytes(r.value)}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Search className="size-5 text-violet" />
            {t('activeInvestigations.title')}
          </CardTitle>
          <CardDescription>
            <Trans
              ns="dashboard"
              i18nKey="activeInvestigations.description"
              components={{ code: <code />, strong: <strong /> }}
            />
          </CardDescription>
        </CardHeader>
        <CardContent>
          {(activeInvQ.data?.rows ?? []).length === 0 ? (
            <div className="text-muted text-sm">{t('activeInvestigations.empty')}</div>
          ) : (
            <div className="space-y-2">
              {(activeInvQ.data?.rows ?? []).map((r) => (
                <div
                  key={r.pc_id}
                  className="flex items-center gap-3 text-sm"
                >
                  <Badge variant="amber" className="shrink-0">
                    {t('activeInvestigations.latest', {
                      age: fmtRelative(r.latest_at),
                    })}
                  </Badge>
                  <Link
                    to={`/agents/${encodeURIComponent(r.pc_id)}`}
                    className="hover:underline"
                  >
                    <code className="text-xs">{r.pc_id}</code>
                  </Link>
                  {r.hostname && (
                    <span className="text-muted text-xs">· {r.hostname}</span>
                  )}
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      {/* v0.37 / agent perf: scan duration aggregates per job_id
          over the last 24 h. Sorted slowest-first so a probe that
          starts hurting jumps to the top of the card without the
          operator having to scan a list. Empty state when no
          finished rows in the window (e.g. fresh install). */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Gauge className="size-5 text-violet" />
            {t('scanDuration.title')}
          </CardTitle>
          <CardDescription>
            <Trans
              ns="dashboard"
              i18nKey="scanDuration.description"
              components={{ code: <code /> }}
            />
          </CardDescription>
        </CardHeader>
        <CardContent>
          {scanDurQ.isLoading ? (
            <div className="text-muted text-sm">{t('actions.loading', { ns: 'common' })}</div>
          ) : scanDurQ.error ? (
            <div className="text-danger text-sm">
              {t('scanDuration.error', { error: String(scanDurQ.error) })}
            </div>
          ) : (scanDurQ.data ?? []).length === 0 ? (
            <div className="text-muted text-sm">
              {t('scanDuration.empty')}
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('scanDuration.table.jobId')}</TableHead>
                  <TableHead className="text-right">{t('scanDuration.table.count')}</TableHead>
                  <TableHead className="text-right">{t('scanDuration.table.p50')}</TableHead>
                  <TableHead className="text-right">{t('scanDuration.table.p95')}</TableHead>
                  <TableHead className="text-right">{t('scanDuration.table.p99')}</TableHead>
                  <TableHead className="text-right">{t('scanDuration.table.max')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(scanDurQ.data ?? []).map((r) => (
                  <TableRow key={r.job_id}>
                    <TableCell>
                      <code className="text-sm">{r.job_id}</code>
                    </TableCell>
                    <TableCell className="text-right text-muted text-xs">{r.count}</TableCell>
                    <TableCell className="text-right">{fmtMs(r.p50_ms)}</TableCell>
                    <TableCell className="text-right">{fmtMs(r.p95_ms)}</TableCell>
                    <TableCell className="text-right">{fmtMs(r.p99_ms)}</TableCell>
                    <TableCell className="text-right text-muted">{fmtMs(r.max_ms)}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Wifi className="size-5 text-violet" />
            {t('resourceDetail.title')}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {js ? (
            <div className="grid grid-cols-1 md:grid-cols-3 gap-4 text-sm">
              {(['streams', 'kv_buckets', 'object_stores'] as const).map((kind) => {
                const kindKey =
                  kind === 'streams' ? 'streams'
                  : kind === 'kv_buckets' ? 'kvBuckets'
                  : 'objectStores';
                return (
                  <div key={kind}>
                    <div className="text-xs uppercase tracking-wide text-muted mb-1">
                      {t(`resourceDetail.kind.${kindKey}`)}
                    </div>
                    <ul className="space-y-1">
                      {js[kind].map((r) => (
                        <li key={r.name} className="flex items-center gap-2">
                          {r.exists ? (
                            <CheckCircle2 className="size-3.5 text-success" />
                          ) : (
                            <XCircle className="size-3.5 text-danger" />
                          )}
                          <code className="text-xs">{r.name}</code>
                        </li>
                      ))}
                    </ul>
                  </div>
                );
              })}
            </div>
          ) : (
            <div className="text-muted text-sm">{t('actions.loading', { ns: 'common' })}</div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
