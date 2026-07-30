import { useQuery } from '@tanstack/react-query';
import {
  Activity,
  AlertTriangle,
  CalendarClock,
  CheckCircle2,
  Clock,
  Cpu,
  Gauge,
  HelpCircle,
  LineChart as LineChartIcon,
  MemoryStick,
  Pin,
  Search,
  ShieldCheck,
  Tags,
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

import { WidgetCard, type Widget } from '@/components/AnalyticsWidget';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { ApiError, apiFetch, formatError } from '@/lib/api';
import { cn } from '@/lib/utils';
import type {
  ActiveInvestigationsResponse,
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
  /** result_id of the slowest run in the window — drives the "max"
   *  cell deep-link to that run's detail page. Null only for legacy
   *  rows with no result_id. */
  max_result_id: string | null;
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
  /** v0.29 detail-route key — distinct from request_id and what the
   *  "recent results" rows deep-link to (`/activity/{result_id}`). */
  result_id: string;
  request_id: string;
  /** The manifest id this run executed, when it came from a registered
   *  job (scheduled or fleet-dispatched). Null for ad-hoc `kanade run`
   *  output. Shown as the human-readable label so the card reads
   *  "app-usage · minipc" instead of an opaque request hash. */
  job_id: string | null;
  pc_id: string;
  exit_code: number;
  started_at: string | null;
  finished_at: string | null;
};

/** One per-check fleet rollup from `GET /api/checks` (`counts`). The
 *  attention rows are omitted on the dashboard — only the tallies feed
 *  the compliance-summary card. */
type CheckCounts = {
  check_name: string;
  label: string | null;
  ok: number;
  warn: number;
  fail: number;
  unknown: number;
};
type ChecksResponse = { counts: CheckCounts[]; rows: unknown[] };

/** `GET /api/agents/versions` — agent-version histogram, busiest first.
 *  `active` is the live subset (heartbeat < 2 min) so the card doubles
 *  as a rollout-coverage read. */
type VersionCount = {
  version: string | null;
  total: number;
  active: number;
};

/** `GET /api/schedules/upcoming` — soonest fires across enabled calendar
 *  schedules, soonest first. */
type UpcomingFire = {
  id: string;
  job_id: string;
  when: string;
  next_run: string;
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

/** Forward-looking sibling of {@link fmtRelative} for the upcoming-
 *  schedules card — "in 2h" rather than "2h ago". A non-positive delta
 *  (a fire that just passed between fetch and render) reads as "now". */
function fmtUntil(iso: string): string {
  const ts = new Date(iso).getTime();
  if (isNaN(ts)) return iso;
  const delta = ts - Date.now();
  if (delta <= 0) return 'now';
  const sec = Math.floor(delta / 1000);
  if (sec < 60) return `in ${sec}s`;
  const min = Math.floor(sec / 60);
  if (min < 60) return `in ${min}m`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `in ${hr}h`;
  return `in ${Math.floor(hr / 24)}d`;
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

// Pinned Analytics widgets over the trailing 24 h, fleet scope. Built at
// fetch time (same sliding-window reason as fleetPerfUrl) so the queryKey
// stays stable. No `pc_id` ⇒ fleet widgets; `pinned=true` ⇒ only the ones
// an operator promoted with `pin_dashboard: true`.
function pinnedAnalyticsUrl(): string {
  const from = new Date(
    Date.now() - FLEET_PERF_WINDOW_HOURS * 60 * 60 * 1000,
  ).toISOString();
  const tzOffset = -new Date().getTimezoneOffset();
  const params = new URLSearchParams({
    pinned: 'true',
    from,
    to: new Date().toISOString(),
    tz_offset_minutes: String(tzOffset),
  });
  return `/api/analytics?${params.toString()}`;
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
  // The pinned-widget cards reuse the Analytics renderer, which labels its
  // sub-parts from the `analytics` namespace (gauge.ratio, timeline.active…).
  const { t: tw } = useTranslation('analytics');
  // Pinned Analytics widgets (`pin_dashboard: true`), fleet scope, 24 h.
  // Empty/absent ⇒ the section doesn't render, so a fleet that pins nothing
  // sees no change.
  const pinnedQ = useQuery({
    // Static key; the URL is rebuilt with a fresh sliding 24h window inside
    // queryFn on each refetch — same pattern as the fleet-perf queries.
    queryKey: ['analytics-pinned'],
    queryFn: () => apiFetch<Widget[]>(pinnedAnalyticsUrl()),
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
  // Compliance summary card — reuses the Compliance page's `/api/checks`
  // rollup (its `counts` carry fleet-true ok/warn/fail/unknown per check,
  // so we never fetch the heavy per-PC rows here).
  const checksQ = useQuery({
    queryKey: ['checks-summary'],
    queryFn: () => apiFetch<ChecksResponse>('/api/checks'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  // Version-distribution card — fleet-wide agent-version histogram.
  const versionsQ = useQuery({
    queryKey: ['agent-versions'],
    queryFn: () => apiFetch<VersionCount[]>('/api/agents/versions'),
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  // Upcoming-schedules card — the soonest fires across enabled calendar
  // schedules.
  const upcomingQ = useQuery({
    queryKey: ['schedules-upcoming'],
    queryFn: () => apiFetch<UpcomingFire[]>('/api/schedules/upcoming?limit=6'),
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
          // #522: a proxy/gateway 503 carries an HTML body, not the
          // degraded-rollup JSON — surface the original error
          // instead of letting JSON.parse throw into the query.
          try {
            return JSON.parse(e.body) as FleetHealth;
          } catch {
            throw e;
          }
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

  // `js` still backs the "resource detail" card at the bottom; the old
  // Fleet / JetStream summary cards (and their agents/jsRows derivations)
  // were dropped as duplicates of the fleet-health rollup up top.
  const js = jsQ.data;

  // Fullest capped resource across every stream / store — turns the
  // JetStream card from a bare green "5/5" into "how close to trimming".
  const jsPeak = (() => {
    if (!js) return null;
    const all = [...js.streams, ...js.object_stores, ...js.kv_buckets];
    let best: { name: string; pct: number } | null = null;
    for (const r of all) {
      if (r.bytes != null && r.max_bytes != null && r.max_bytes > 0) {
        const pct = (r.bytes / r.max_bytes) * 100;
        if (!best || pct > best.pct) best = { name: r.name, pct };
      }
    }
    return best;
  })();

  const recentFail = (resultsQ.data ?? []).filter((r) => r.exit_code !== 0).length;
  const recentTotal = (resultsQ.data ?? []).length;

  // Compliance rollup: fold the per-check counts into a single
  // attention-vs-ok read. `attention` = any check with at least one
  // warn / fail / unknown PC; the card lists those first.
  const checkCounts = checksQ.data?.counts ?? [];
  const checksAttention = checkCounts.filter(
    (c) => c.fail > 0 || c.warn > 0 || c.unknown > 0,
  ).length;
  // Attention checks (any warn/fail/unknown) float to the top, then
  // alphabetical for a stable order. Sorted on a fresh copy here — the
  // JSX must not `.sort()` `checkCounts` in place, since that aliases
  // the TanStack Query cache (claude #883).
  const checkCountsSorted = [...checkCounts].sort((a, b) => {
    const aAtt = a.fail + a.warn + a.unknown > 0 ? 1 : 0;
    const bAtt = b.fail + b.warn + b.unknown > 0 ? 1 : 0;
    if (aAtt !== bAtt) return bAtt - aAtt;
    return a.check_name.localeCompare(b.check_name);
  });

  const versions = versionsQ.data ?? [];
  // Longest bar = busiest build; floor at 1 so an all-zero histogram
  // (shouldn't happen) doesn't divide by zero.
  const maxVersionTotal = Math.max(...versions.map((v) => v.total), 1);

  // #522: errors used to coalesce into `?? []` / conditional
  // rendering, so an API outage rendered as a healthy-looking idle
  // fleet ("Known 0", empty lists). One page-level banner names
  // every failing query instead; the cards below keep their last
  // known data.
  const failedQueries: Array<[string, Error]> = (
    [
      ['jetstream', jsQ.error],
      ['results', resultsQ.error],
      ['audit', auditQ.error],
      ['health', healthQ.error],
      ['checks', checksQ.error],
      ['versions', versionsQ.error],
      ['upcoming', upcomingQ.error],
      ['pinned', pinnedQ.error],
      ['fleetCpu', fleetCpuQ.error],
      ['fleetMem', fleetMemQ.error],
      ['topCpu', topCpuQ.error],
      ['topMem', topMemQ.error],
      ['activeInvestigations', activeInvQ.error],
    ] as Array<[string, Error | null]>
  ).filter((pair): pair is [string, Error] => pair[1] != null);

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

      {failedQueries.length > 0 && (
        <Card className="border-danger">
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-danger">
              <AlertTriangle className="size-5" />
              {t('degraded.title', { count: failedQueries.length })}
            </CardTitle>
            <CardDescription>{t('degraded.description')}</CardDescription>
          </CardHeader>
          <CardContent className="space-y-1">
            {failedQueries.map(([name, err]) => (
              <div key={name} className="text-sm">
                <span className="font-medium">{t(`degraded.sources.${name}`)}</span>
                <span className="text-muted"> — {formatError(err)}</span>
              </div>
            ))}
          </CardContent>
        </Card>
      )}

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
            {/* The "active / known" tile is about the ACTIVE hosts, so it
                deep-links to the online filter — the count you clicked is
                exactly the list you land on. (The stale tile next to it
                owns the offline drill-down.) */}
            <Link
              to="/agents?status=online"
              className="rounded-md -m-1 p-1 transition-colors hover:bg-muted/10"
              title={t('fleetHealth.stats.agents.linkTitle')}
            >
              <StatBlock
                label={t('fleetHealth.stats.agents.label')}
                value={`${health.agents.active} / ${health.agents.known}`}
                hint={t('fleetHealth.stats.agents.hint')}
                // A single offline PC is normal — don't alarm on it.
                // A total blackout (0 active, some known) is the
                // degraded case, so flag it red.
                tone={health.agents.active === 0 && health.agents.known > 0 ? 'danger' : 'default'}
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
                // Red only on a total blackout; a partial set of
                // stale hosts is expected, so neutral. Green at zero.
                tone={
                  health.agents.active === 0 && health.agents.known > 0
                    ? 'danger'
                    : health.agents.stale > 0
                      ? 'default'
                      : 'success'
                }
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
                  : jsPeak
                    ? t('fleetHealth.stats.jetstream.hintPeak', {
                        pct: Math.round(jsPeak.pct),
                        name: jsPeak.name,
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

      {/* Pinned Analytics widgets — any operator view flagged
          `pin_dashboard: true` surfaces here, rendered with the same
          components as the Analytics page. This is how a config-driven
          dashboard (e.g. a future vulnerability rollup) reaches the home
          page without a bespoke card. Hidden entirely when nothing is
          pinned. */}
      {(pinnedQ.data?.length ?? 0) > 0 && (
        <div className="space-y-3">
          <div className="flex items-baseline justify-between">
            <h3 className="text-lg flex items-center gap-2">
              <Pin className="size-4 text-violet" />
              {t('pinned.title')}
            </h3>
            <Link to="/analytics" className="text-xs text-muted hover:underline">
              {t('pinned.linkAll')}
            </Link>
          </div>
          <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
            {(pinnedQ.data ?? []).map((w, i) => (
              <WidgetCard key={`${w.dashboard}:${w.title}:${i}`} w={w} t={tw} />
            ))}
          </div>
        </div>
      )}

      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        {/* Recent results — the latest job runs across the fleet. Shows
            the JOB NAME (not the opaque request hash it used to), and
            each row deep-links to that run's detail page. */}
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
          <CardContent className="space-y-1">
            {(resultsQ.data ?? []).map((r) => (
              <Link
                key={r.result_id}
                to={`/activity/${encodeURIComponent(r.result_id)}`}
                className="flex items-center gap-3 text-sm rounded-md -mx-1 px-1 py-1 transition-colors hover:bg-muted/10"
                title={t('recentResults.rowTitle')}
              >
                {r.exit_code === 0 ? (
                  <CheckCircle2 className="size-4 text-success shrink-0" />
                ) : (
                  <XCircle className="size-4 text-danger shrink-0" />
                )}
                {/* Job name is the human-readable label; ad-hoc `kanade
                    run` output has no job_id, so fall back to the short
                    request hash. */}
                {r.job_id ? (
                  <span className="font-medium truncate">{r.job_id}</span>
                ) : (
                  <code className="text-xs">{r.request_id.slice(0, 8)}</code>
                )}
                <span className="text-muted">·</span>
                <code className="text-xs text-muted">{r.pc_id}</code>
                <span className="text-muted text-xs ml-auto whitespace-nowrap">
                  {fmtRelative(r.finished_at)}
                </span>
              </Link>
            ))}
            {(resultsQ.data ?? []).length === 0 && (
              <div className="text-muted text-sm">{t('recentResults.empty')}</div>
            )}
          </CardContent>
        </Card>

        {/* Recent activity — the audit feed. Each row deep-links: exec
            events to that job's runs, everything else to the full audit
            log. */}
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Clock className="size-5 text-violet" />
              {t('recentActivity.title')}
            </CardTitle>
            <CardDescription>{t('recentActivity.description')}</CardDescription>
          </CardHeader>
          <CardContent className="space-y-1">
            {(auditQ.data ?? []).map((e) => (
              <Link
                key={e.id}
                to={
                  e.action === 'exec' && e.target
                    ? `/activity?job_id=${encodeURIComponent(e.target)}`
                    : '/audit'
                }
                className="flex items-center gap-3 text-sm rounded-md -mx-1 px-1 py-1 transition-colors hover:bg-muted/10"
                title={t('recentActivity.rowTitle')}
              >
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
              </Link>
            ))}
            {(auditQ.data ?? []).length === 0 && (
              <div className="text-muted text-sm">{t('recentActivity.empty')}</div>
            )}
          </CardContent>
        </Card>

        {/* Compliance summary — fleet-wide health-check rollup, attention
            checks first. Reuses the Compliance page's `/api/checks`
            counts; the whole card deep-links there. */}
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <ShieldCheck className="size-5 text-violet" />
              {t('compliance.title')}
            </CardTitle>
            <CardDescription>
              {checkCounts.length === 0
                ? t('compliance.descriptionEmpty')
                : checksAttention > 0
                  ? t('compliance.descriptionAttention', {
                      attention: checksAttention,
                      total: checkCounts.length,
                    })
                  : t('compliance.descriptionAllGreen', { total: checkCounts.length })}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-1">
            {checkCountsSorted
              .slice(0, 6)
              .map((c) => (
                <Link
                  key={c.check_name}
                  to="/compliance"
                  className="flex items-center gap-2 text-sm rounded-md -mx-1 px-1 py-1 transition-colors hover:bg-muted/10"
                  title={t('compliance.rowTitle')}
                >
                  {c.fail + c.warn + c.unknown === 0 ? (
                    <CheckCircle2 className="size-4 text-success shrink-0" />
                  ) : (
                    <AlertTriangle className="size-4 text-danger shrink-0" />
                  )}
                  <span className="truncate">{c.label ?? c.check_name}</span>
                  <span className="ml-auto flex items-center gap-1 shrink-0 tabular-nums">
                    {c.fail > 0 && <Badge variant="danger">{t('compliance.fail', { n: c.fail })}</Badge>}
                    {c.warn > 0 && <Badge variant="amber">{t('compliance.warn', { n: c.warn })}</Badge>}
                    {c.unknown > 0 && <Badge variant="default">{t('compliance.unknown', { n: c.unknown })}</Badge>}
                    <span className="text-muted text-xs">{t('compliance.ok', { n: c.ok })}</span>
                  </span>
                </Link>
              ))}
            {checkCounts.length === 0 && (
              <div className="text-muted text-sm">{t('compliance.empty')}</div>
            )}
          </CardContent>
        </Card>

        {/* Upcoming schedules — the soonest fires across enabled calendar
            schedules. "What runs next." Deep-links to the Schedules page. */}
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <CalendarClock className="size-5 text-violet" />
              {t('upcoming.title')}
            </CardTitle>
            <CardDescription>{t('upcoming.description')}</CardDescription>
          </CardHeader>
          <CardContent className="space-y-1">
            {(upcomingQ.data ?? []).map((u) => (
              <Link
                key={`${u.id}/${u.next_run}`}
                to="/schedules"
                className="flex items-center gap-3 text-sm rounded-md -mx-1 px-1 py-1 transition-colors hover:bg-muted/10"
                title={t('upcoming.rowTitle')}
              >
                <span className="font-medium truncate">{u.job_id}</span>
                <code className="text-muted text-[10px] truncate">{u.when}</code>
                <span
                  className="text-muted text-xs ml-auto whitespace-nowrap"
                  title={fmtAxisTime(u.next_run)}
                >
                  {fmtUntil(u.next_run)}
                </span>
              </Link>
            ))}
            {(upcomingQ.data ?? []).length === 0 && (
              <div className="text-muted text-sm">{t('upcoming.empty')}</div>
            )}
          </CardContent>
        </Card>

        {/* Version distribution — agent-version histogram, busiest build
            first. The bar reads as rollout coverage at a glance; click
            through to the Rollout page for the per-version drill-down. */}
        <Card className="md:col-span-2">
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Tags className="size-5 text-violet" />
              {t('versions.title')}
            </CardTitle>
            <CardDescription>{t('versions.description')}</CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            {versions.length === 0 ? (
              <div className="text-muted text-sm">{t('versions.empty')}</div>
            ) : (
              versions.map((v) => (
                <Link
                  key={v.version ?? '__unknown__'}
                  to="/rollout"
                  className="flex items-center gap-3 text-sm rounded-md -mx-1 px-1 py-1 transition-colors hover:bg-muted/10"
                  title={t('versions.rowTitle')}
                >
                  <code className="w-28 shrink-0 text-xs truncate">
                    {v.version ?? t('versions.unknown')}
                  </code>
                  <div className="flex-1 h-2 rounded-full bg-muted/20 overflow-hidden">
                    <div
                      className="h-full rounded-full bg-violet"
                      style={{ width: `${(v.total / maxVersionTotal) * 100}%` }}
                    />
                  </div>
                  <span className="w-28 shrink-0 text-right text-xs text-muted tabular-nums">
                    {t('versions.activeOfTotal', { active: v.active, total: v.total })}
                  </span>
                </Link>
              ))
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
                      <TableCell label={t('topTable.pcId')}>
                        <Link
                          to={`/agents/${encodeURIComponent(r.pc_id)}`}
                          className="hover:underline"
                        >
                          <code className="text-xs">{r.pc_id}</code>
                        </Link>
                      </TableCell>
                      <TableCell label={t('topTable.hostname')} className="text-muted text-xs">
                        {r.hostname ?? '—'}
                      </TableCell>
                      <TableCell label={t('topTable.value')} className="text-right tabular-nums">
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
                      <TableCell label={t('topTable.pcId')}>
                        <Link
                          to={`/agents/${encodeURIComponent(r.pc_id)}`}
                          className="hover:underline"
                        >
                          <code className="text-xs">{r.pc_id}</code>
                        </Link>
                      </TableCell>
                      <TableCell label={t('topTable.hostname')} className="text-muted text-xs">
                        {r.hostname ?? '—'}
                      </TableCell>
                      <TableCell label={t('topTable.value')} className="text-right tabular-nums">
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
                    <TableCell label={t('scanDuration.table.jobId')}>
                      {/* Deep-link into Activity filtered to this job —
                          one click from "this probe is slow" to its
                          actual runs, the same job_id bridge the Jobs
                          live chip uses. */}
                      <Link
                        to={`/activity?job_id=${encodeURIComponent(r.job_id)}`}
                        className="hover:underline"
                        title={t('scanDuration.linkTitle')}
                      >
                        <code className="text-sm">{r.job_id}</code>
                      </Link>
                    </TableCell>
                    <TableCell label={t('scanDuration.table.count')} className="text-right text-muted text-xs">{r.count}</TableCell>
                    <TableCell label={t('scanDuration.table.p50')} className="text-right">{fmtMs(r.p50_ms)}</TableCell>
                    <TableCell label={t('scanDuration.table.p95')} className="text-right">{fmtMs(r.p95_ms)}</TableCell>
                    <TableCell label={t('scanDuration.table.p99')} className="text-right">{fmtMs(r.p99_ms)}</TableCell>
                    <TableCell label={t('scanDuration.table.max')} className="text-right text-muted">
                      {/* Deep-link the slowest single run straight to
                          its detail page — "the max one, take me to
                          it". Falls back to plain text for legacy rows
                          with no result_id. */}
                      {r.max_result_id ? (
                        <Link
                          to={`/activity/${encodeURIComponent(r.max_result_id)}`}
                          className="hover:underline"
                          title={t('scanDuration.maxLinkTitle')}
                        >
                          {fmtMs(r.max_ms)}
                        </Link>
                      ) : (
                        fmtMs(r.max_ms)
                      )}
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
