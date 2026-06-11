import { useMutation, useQuery } from '@tanstack/react-query';
import { Activity, Loader2, ScrollText, Server, Settings2, Users } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, apiFetchPaged } from '@/lib/api';
import { useDebouncedValue } from '@/lib/hooks';
import type { AgentGroups, AgentRow, EffectiveConfigResponse, Heartbeat } from '@/lib/types';
import { cn, fmtIsoLocal, isAgentOnline } from '@/lib/utils';

// #495: server-side page size. The endpoint supports q/limit/offset;
// 50 rows keeps the polled payload and the rendered DOM bounded
// regardless of fleet size (the page previously rendered the whole
// fleet every 30 s tick).
const PAGE_SIZE = 50;
// Same debounce the other list pages use for typed filters.
const FILTER_DEBOUNCE_MS = 300;

// Liveness filter for the list, shared with the URL `?status=` param
// so the Dashboard's fleet-health tiles can deep-link straight to the
// offline hosts ("2 / 4 — which 2?"). `all` is the default / no-param
// state.
type StatusFilter = 'all' | 'online' | 'offline';

function parseStatusFilter(raw: string | null): StatusFilter {
  return raw === 'online' || raw === 'offline' ? raw : 'all';
}

type ActionResult = {
  pc_id: string;
  action: string;
  value: unknown;
};

/** v0.37 Part 2: per-cell formatters for the perf columns. Null
 *  values render as em-dash so pre-0.37 agents (which don't carry
 *  the fields) leave the column visibly blank rather than `0`. */
function fmtPct(v: number | null): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '—';
  return `${v.toFixed(1)}%`;
}

function fmtBytes(v: number | null): string {
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

export function Agents() {
  const { t } = useTranslation('agents');
  const [q, setQ] = useState('');
  const [offset, setOffset] = useState(0);
  const dQ = useDebouncedValue(q, FILTER_DEBOUNCE_MS);
  const { data, error, isLoading } = useQuery({
    queryKey: ['agents', dQ, offset],
    // Match the Dashboard cadence so the per-row online/offline badge
    // ages out a dropped agent within ~30s of the fleet-health tile.
    queryFn: () =>
      apiFetchPaged<AgentRow[]>(
        `/api/agents?limit=${PAGE_SIZE}&offset=${offset}${dQ ? `&q=${encodeURIComponent(dQ)}` : ''}`,
      ),
    refetchInterval: 30_000,
  });
  const total = data?.total ?? 0;
  const [result, setResult] = useState<ActionResult | null>(null);
  // #495 (was: one shared mutation.isPending greyed out the action
  // buttons on every row): track pending pc_ids in a Set so only the
  // row actually in flight disables — the pattern Jobs/Activity use.
  const [pendingPcs, setPendingPcs] = useState<Set<string>>(new Set());
  const markPending = (pcId: string, on: boolean) =>
    setPendingPcs((prev) => {
      const next = new Set(prev);
      if (on) next.add(pcId);
      else next.delete(pcId);
      return next;
    });
  const [searchParams, setSearchParams] = useSearchParams();
  const statusFilter = parseStatusFilter(searchParams.get('status'));

  // A new search (or status-chip change) resets to page 1 — a stale
  // offset against a narrower result set would show an empty page.
  // Adjusted during render (not useEffect) so the reset lands BEFORE
  // the query fires, avoiding one wasted fetch at the old offset
  // (PR #559 review, gemini + claude).
  const [prevFilterKey, setPrevFilterKey] = useState({ dQ, statusFilter });
  if (prevFilterKey.dQ !== dQ || prevFilterKey.statusFilter !== statusFilter) {
    setPrevFilterKey({ dQ, statusFilter });
    setOffset(0);
  }

  const setStatusFilter = (next: StatusFilter) => {
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        if (next === 'all') p.delete('status');
        else p.set('status', next);
        return p;
      },
      { replace: true },
    );
  };

  const ping = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch<{ heartbeat: Heartbeat }>(`/api/agents/${encodeURIComponent(pcId)}/ping`, {
        method: 'POST',
      }),
  });
  const effective = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch<EffectiveConfigResponse>(`/api/agents/${encodeURIComponent(pcId)}/effective_config`),
  });
  const groupsGet = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch<AgentGroups>(`/api/agents/${encodeURIComponent(pcId)}/groups`),
  });
  const groupsPut = useMutation({
    mutationFn: ({ pcId, groups }: { pcId: string; groups: string[] }) =>
      apiFetch<AgentGroups>(`/api/agents/${encodeURIComponent(pcId)}/groups`, {
        method: 'PUT',
        body: JSON.stringify({ groups }),
      }),
  });

  const doPing = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'ping', value: '…' });
    markPending(pcId, true);
    try {
      const r = await ping.mutateAsync(pcId);
      setResult({ pc_id: pcId, action: 'ping', value: r });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'ping', value: (e as Error).message });
    } finally {
      markPending(pcId, false);
    }
  };
  const doEffective = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'effective', value: '…' });
    markPending(pcId, true);
    try {
      const r = await effective.mutateAsync(pcId);
      setResult({ pc_id: pcId, action: 'effective', value: r });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'effective', value: (e as Error).message });
    } finally {
      markPending(pcId, false);
    }
  };
  const doGroups = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'groups', value: t('groupsLoading') });
    markPending(pcId, true);
    try {
      const current = await groupsGet.mutateAsync(pcId);
      const next = window.prompt(
        t('groupsPrompt', {
          pcId,
          current: current.groups.join(', ') || t('groupsNone'),
        }),
        current.groups.join(', '),
      );
      if (next === null) {
        setResult({ pc_id: pcId, action: 'groups', value: t('groupsCancelled') });
        return;
      }
      const list = next.split(',').map((s) => s.trim()).filter(Boolean);
      const updated = await groupsPut.mutateAsync({ pcId, groups: list });
      setResult({ pc_id: pcId, action: 'groups', value: updated });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'groups', value: (e as Error).message });
    } finally {
      markPending(pcId, false);
    }
  };

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />
        {t('loading')}
      </div>
    );
  }
  if (error) return <ErrorCard title={t('errorTitle')} error={error} />;
  const agents = data?.rows ?? [];
  // One `now` snapshot for the whole render so the counts, the filter,
  // and the per-row badges below all agree on liveness for an agent
  // sitting right on the 2-min threshold.
  const now = Date.now();
  const onlineCount = agents.filter((a) => isAgentOnline(a.last_heartbeat, now)).length;
  const offlineCount = agents.length - onlineCount;
  const visible = agents.filter((a) => {
    if (statusFilter === 'all') return true;
    const online = isAgentOnline(a.last_heartbeat, now);
    return statusFilter === 'online' ? online : !online;
  });

  // Only the genuinely-empty fleet gets the onboarding card — a
  // filtered-empty page (or an out-of-range offset) keeps the table
  // chrome so the operator can clear the search / page back.
  if (total === 0 && !dQ) {
    return (
      <Card>
        <CardHeader className="items-center text-center">
          <Server className="size-10 text-muted" />
          <CardTitle>{t('empty.title')}</CardTitle>
        </CardHeader>
        <CardContent className="text-center text-muted">
          {t('empty.body')}
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <Badge variant="violet">{t('countBadge', { count: total })}</Badge>
      </div>
      <p className="text-xs text-muted">
        <Trans
          ns="agents"
          i18nKey="intro"
          components={{
            inventoryLink: <Link to="/inventory" className="underline" />,
            strong: <strong />,
          }}
        />
      </p>

      {/* Liveness filter — the bridge to the Dashboard's "active /
          known" tile. Counts come from the same `isAgentOnline`
          threshold, so toggling "offline" answers "which hosts are
          the N that aren't connected?" directly. */}
      <div className="flex flex-wrap items-center gap-2">
        <Input
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder={t('searchPlaceholder')}
          className="h-8 w-64"
        />
        {(['all', 'online', 'offline'] as const).map((s) => {
          // 'All' shows the fleet-wide match count (the same number
          // as the badge above); online/offline stay page-local —
          // the acknowledged trade-off until a server-side status
          // filter lands (PR #559 review, claude).
          const count = s === 'online' ? onlineCount : s === 'offline' ? offlineCount : total;
          const selected = statusFilter === s;
          return (
            <button
              key={s}
              type="button"
              onClick={() => setStatusFilter(s)}
              aria-pressed={selected}
              className={cn(
                'rounded-full border px-3 py-1 text-xs font-medium transition-colors',
                selected
                  ? s === 'offline'
                    ? 'border-transparent bg-danger/15 text-danger'
                    : s === 'online'
                      ? 'border-transparent bg-success/15 text-success'
                      : 'border-transparent bg-violet/15 text-violet'
                  : 'border-border text-muted hover:bg-muted/10',
              )}
            >
              {t(`filter.${s}`, { count })}
            </button>
          );
        })}
      </div>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>{t('columns.status')}</TableHead>
            <TableHead>{t('columns.pcId')}</TableHead>
            <TableHead>{t('columns.hostname')}</TableHead>
            <TableHead>{t('columns.os')}</TableHead>
            <TableHead>{t('columns.agent')}</TableHead>
            <TableHead>{t('columns.lastHeartbeat')}</TableHead>
            {/* v0.37 Part 2: agent process self-perf columns. Pre-
                0.37 agents leave these null and the cell renders
                as an em-dash, so the table stays usable during a
                rolling upgrade. Headers are prefixed "agent" so the
                columns are clearly the agent process, not the host. */}
            <TableHead className="text-right" title={t('columnTitles.cpu')}>
              {t('columns.cpu')}
            </TableHead>
            <TableHead className="text-right" title={t('columnTitles.rss')}>
              {t('columns.rss')}
            </TableHead>
            <TableHead>{t('columns.actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {visible.map((a) => {
            const online = isAgentOnline(a.last_heartbeat, now);
            return (
            <TableRow key={a.pc_id}>
              <TableCell>
                <Badge
                  variant={online ? 'success' : 'danger'}
                  title={t(online ? 'status.onlineTitle' : 'status.offlineTitle')}
                >
                  <span
                    className={cn(
                      'mr-1.5 inline-block size-1.5 rounded-full',
                      online ? 'bg-success' : 'bg-danger',
                    )}
                  />
                  {t(online ? 'status.online' : 'status.offline')}
                </Badge>
              </TableCell>
              <TableCell>
                <Link
                  to={`/agents/${encodeURIComponent(a.pc_id)}`}
                  className="hover:underline"
                  title={t('rowLinkTitle')}
                >
                  <code className="text-xs">{a.pc_id}</code>
                </Link>
              </TableCell>
              <TableCell>{a.hostname ?? <span className="text-muted">—</span>}</TableCell>
              <TableCell className="text-muted text-xs">{a.os_family ?? '—'}</TableCell>
              <TableCell className="text-muted text-xs">{a.agent_version ?? '—'}</TableCell>
              <TableCell className="text-muted text-xs">{fmtIsoLocal(a.last_heartbeat)}</TableCell>
              <TableCell className="text-right text-muted text-xs">{fmtPct(a.agent_cpu_pct)}</TableCell>
              <TableCell className="text-right text-muted text-xs">{fmtBytes(a.agent_rss_bytes)}</TableCell>
              <TableCell>
                <div className="flex gap-1 flex-wrap">
                  <Button variant="secondary" size="sm" asChild>
                    <Link to={`/inventory?pc=${encodeURIComponent(a.pc_id)}`}>
                      <ScrollText className="size-3.5" />{t('actions.facts')}
                    </Link>
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doPing(a.pc_id)} disabled={pendingPcs.has(a.pc_id)}>
                    <Activity className="size-3.5" />{t('actions.ping')}
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doGroups(a.pc_id)} disabled={pendingPcs.has(a.pc_id)}>
                    <Users className="size-3.5" />{t('actions.groups')}
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doEffective(a.pc_id)} disabled={pendingPcs.has(a.pc_id)}>
                    <Settings2 className="size-3.5" />{t('actions.effective')}
                  </Button>
                </div>
              </TableCell>
            </TableRow>
            );
          })}
          {visible.length === 0 && (
            <TableRow>
              <TableCell colSpan={9} className="text-center text-muted text-sm py-6">
                {t('filterEmpty')}
              </TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>

      {(offset > 0 || total > PAGE_SIZE) && (
        <div className="flex items-center gap-2">
          <Button
            variant="secondary"
            size="sm"
            onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
            disabled={offset === 0}
          >
            {t('prev')}
          </Button>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => setOffset(offset + PAGE_SIZE)}
            disabled={offset + PAGE_SIZE >= total}
          >
            {t('next')}
          </Button>
          <span className="text-xs text-muted">
            {t('pageRange', {
              from: Math.min(offset + 1, total),
              to: Math.min(offset + PAGE_SIZE, total),
              total,
            })}
          </span>
        </div>
      )}

      {result && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">
              <code className="text-xs mr-2">{result.pc_id}</code>
              <Badge variant="amber">{result.action}</Badge>
            </CardTitle>
          </CardHeader>
          <CardContent>
            <JsonOutput value={result.value} />
          </CardContent>
        </Card>
      )}
    </div>
  );
}
