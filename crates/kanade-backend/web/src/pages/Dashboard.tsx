import { useQuery } from '@tanstack/react-query';
import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  Clock,
  Gauge,
  HelpCircle,
  Server,
  Users,
  Wifi,
  XCircle,
} from 'lucide-react';

import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';

import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { ApiError, apiFetch } from '@/lib/api';
import { cn } from '@/lib/utils';
import type { AgentRow, JetstreamSnapshot } from '@/lib/types';

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

// 2 min — covers the default 30s heartbeat cadence with several
// missed ticks of slack. Heartbeat is the cheaper / faster liveness
// signal than the 24h inventory cycle, so the dashboard reacts to
// an agent dropping offline within ~2 min instead of ~25h.
const ACTIVE_THRESHOLD_MS = 2 * 60 * 1000;

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
  const agentsQ = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
    refetchInterval: 30_000,
  });
  const jsQ = useQuery({
    queryKey: ['jetstream-status'],
    queryFn: () => apiFetch<JetstreamSnapshot>('/api/jetstream/status'),
    refetchInterval: 30_000,
  });
  const resultsQ = useQuery({
    queryKey: ['results-recent'],
    queryFn: () => apiFetch<ResultRow[]>('/api/results?limit=8'),
    refetchInterval: 30_000,
  });
  const auditQ = useQuery({
    queryKey: ['audit-recent'],
    queryFn: () => apiFetch<AuditRow[]>('/api/audit?limit=8'),
    refetchInterval: 30_000,
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
    refetchInterval: 30_000,
  });
  const scanDurQ = useQuery({
    queryKey: ['scan-durations'],
    queryFn: () => apiFetch<ScanDurationStats[]>('/api/health/scan_durations'),
    refetchInterval: 60_000,
  });

  const agents = agentsQ.data ?? [];
  const active = agents.filter(
    (a) =>
      a.last_heartbeat &&
      Date.now() - new Date(a.last_heartbeat).getTime() < ACTIVE_THRESHOLD_MS,
  ).length;

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
          ok:       { icon: CheckCircle2,  tone: 'success' as const, label: 'all green',     hint: 'agents fresh, JetStream healthy' },
          unknown:  { icon: HelpCircle,    tone: 'default' as const, label: 'unknown',       hint: 'no agents reporting yet' },
          degraded: { icon: AlertTriangle, tone: 'danger'  as const, label: 'degraded',      hint: 'fleet attention needed' },
        }
      )[health.status]
    : null;

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-2xl">Dashboard</h2>
        <span className="text-xs text-muted">auto-refresh: 30s</span>
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
              Fleet health
              <Badge
                variant={
                  healthMeta.tone === 'success' ? 'success'
                  : healthMeta.tone === 'danger' ? 'danger'
                  : 'amber'
                }
              >
                {healthMeta.label}
              </Badge>
            </CardTitle>
            <CardDescription>
              From <code>/api/health/fleet</code> · {healthMeta.hint}
            </CardDescription>
          </CardHeader>
          <CardContent className="grid grid-cols-2 md:grid-cols-4 gap-6">
            <StatBlock
              label="agents"
              value={`${health.agents.active} / ${health.agents.known}`}
              hint="active / known"
              tone={health.agents.stale > 0 ? 'danger' : 'default'}
            />
            <StatBlock
              label="stale agents"
              value={health.agents.stale}
              tone={health.agents.stale > 0 ? 'danger' : 'success'}
              hint="last inventory ≥ 25h"
            />
            <StatBlock
              label="JetStream"
              value={`${health.jetstream.healthy} / ${health.jetstream.total}`}
              tone={health.jetstream.all_ok ? 'success' : 'danger'}
              hint={
                health.jetstream.missing.length > 0
                  ? `missing: ${health.jetstream.missing.join(', ')}`
                  : 'streams + KV + obj store'
              }
            />
            <StatBlock
              label={`failures / ${health.recent_results.window_hours}h`}
              value={`${health.recent_results.failed} / ${health.recent_results.total}`}
              tone={health.recent_results.failed > 0 ? 'danger' : 'default'}
              hint="exit_code ≠ 0 / runs"
            />
          </CardContent>
        </Card>
      )}

      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Users className="size-5 text-violet" />
              Fleet
            </CardTitle>
            <CardDescription>Snapshot from <code>/api/agents</code></CardDescription>
          </CardHeader>
          <CardContent className="flex gap-8 items-end">
            <StatBlock label="known" value={agents.length} />
            <StatBlock
              label="active (heartbeat &lt; 2m)"
              value={active}
              tone={active === agents.length && agents.length > 0 ? 'success' : 'default'}
              hint="based on last heartbeat (~30s cadence)"
            />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Server className="size-5 text-violet" />
              JetStream
            </CardTitle>
            <CardDescription>From <code>/api/jetstream/status</code></CardDescription>
          </CardHeader>
          <CardContent className="flex gap-8 items-end">
            <StatBlock
              label="resources"
              value={js ? `${jsOk} / ${jsRows.length}` : '—'}
              tone={jsAllOk ? 'success' : js ? 'danger' : 'default'}
              hint="streams + KV + object stores"
            />
            {js && (
              <div className="flex flex-col gap-1">
                {jsAllOk ? (
                  <Badge variant="success">all healthy</Badge>
                ) : (
                  <Badge variant="danger">
                    {jsRows.length - jsOk} missing
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
              Recent results
            </CardTitle>
            <CardDescription>
              Last {recentTotal} · {recentFail > 0 ? `${recentFail} failed` : 'all green'}
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
              <div className="text-muted text-sm">No results yet.</div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Clock className="size-5 text-violet" />
              Recent activity
            </CardTitle>
            <CardDescription>Audit feed</CardDescription>
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
              <div className="text-muted text-sm">No audit events yet.</div>
            )}
          </CardContent>
        </Card>
      </div>

      {/* v0.37 / agent perf: scan duration aggregates per job_id
          over the last 24 h. Sorted slowest-first so a probe that
          starts hurting jumps to the top of the card without the
          operator having to scan a list. Empty state when no
          finished rows in the window (e.g. fresh install). */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Gauge className="size-5 text-violet" />
            Scan duration (last 24 h, slowest first)
          </CardTitle>
          <CardDescription>
            Per-job execution time from <code>execution_results.{`{started,finished}_at`}</code>.
            Use to spot a probe that's gotten unexpectedly slow without checking individual rows.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {scanDurQ.isLoading ? (
            <div className="text-muted text-sm">Loading…</div>
          ) : scanDurQ.error ? (
            <div className="text-danger text-sm">
              Couldn't load: {String(scanDurQ.error)}
            </div>
          ) : (scanDurQ.data ?? []).length === 0 ? (
            <div className="text-muted text-sm">
              No finished executions in the last 24 h.
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>job_id</TableHead>
                  <TableHead className="text-right">count</TableHead>
                  <TableHead className="text-right">p50</TableHead>
                  <TableHead className="text-right">p95</TableHead>
                  <TableHead className="text-right">p99</TableHead>
                  <TableHead className="text-right">max</TableHead>
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
            Resource detail
          </CardTitle>
        </CardHeader>
        <CardContent>
          {js ? (
            <div className="grid grid-cols-1 md:grid-cols-3 gap-4 text-sm">
              {(['streams', 'kv_buckets', 'object_stores'] as const).map((kind) => (
                <div key={kind}>
                  <div className="text-xs uppercase tracking-wide text-muted mb-1">{kind.replace('_', ' ')}</div>
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
              ))}
            </div>
          ) : (
            <div className="text-muted text-sm">Loading…</div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
