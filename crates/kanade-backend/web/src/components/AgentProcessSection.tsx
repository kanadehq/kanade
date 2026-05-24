import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Activity, Loader2, Power, RefreshCw } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

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
        {rows.length > 0 && (
          <div className="space-y-2">
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
