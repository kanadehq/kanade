import { useMutation, useQuery } from '@tanstack/react-query';
import { Activity, Loader2, ScrollText, Server, Settings2, Users } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { Link } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { JsonOutput } from '@/components/ui/json-output';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { AgentGroups, AgentRow, EffectiveConfigResponse, Heartbeat } from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

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
  const { data, error, isLoading } = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
  });
  const [result, setResult] = useState<ActionResult | null>(null);

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
    try {
      const r = await ping.mutateAsync(pcId);
      setResult({ pc_id: pcId, action: 'ping', value: r });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'ping', value: (e as Error).message });
    }
  };
  const doEffective = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'effective', value: '…' });
    try {
      const r = await effective.mutateAsync(pcId);
      setResult({ pc_id: pcId, action: 'effective', value: r });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'effective', value: (e as Error).message });
    }
  };
  const doGroups = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'groups', value: t('groupsLoading') });
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
  const agents = data ?? [];

  if (agents.length === 0) {
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
        <Badge variant="violet">{t('countBadge', { count: agents.length })}</Badge>
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
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>{t('columns.pcId')}</TableHead>
            <TableHead>{t('columns.hostname')}</TableHead>
            <TableHead>{t('columns.os')}</TableHead>
            <TableHead>{t('columns.agent')}</TableHead>
            <TableHead>{t('columns.lastHeartbeat')}</TableHead>
            {/* v0.37 Part 2: agent process self-perf columns. Pre-
                0.37 agents leave these null and the cell renders
                as an em-dash, so the table stays usable during a
                rolling upgrade. */}
            <TableHead className="text-right" title={t('columnTitles.cpu')}>
              {t('columns.cpu')}
            </TableHead>
            <TableHead className="text-right" title={t('columnTitles.rss')}>
              {t('columns.rss')}
            </TableHead>
            <TableHead className="text-right" title={t('columnTitles.rd')}>
              {t('columns.rd')}
            </TableHead>
            <TableHead className="text-right" title={t('columnTitles.wr')}>
              {t('columns.wr')}
            </TableHead>
            <TableHead>{t('columns.actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {agents.map((a) => (
            <TableRow key={a.pc_id}>
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
              <TableCell className="text-right text-muted text-xs">{fmtBytes(a.agent_disk_read_bytes)}</TableCell>
              <TableCell className="text-right text-muted text-xs">{fmtBytes(a.agent_disk_written_bytes)}</TableCell>
              <TableCell>
                <div className="flex gap-1 flex-wrap">
                  <Button variant="secondary" size="sm" asChild>
                    <Link to={`/inventory?pc=${encodeURIComponent(a.pc_id)}`}>
                      <ScrollText className="size-3.5" />{t('actions.facts')}
                    </Link>
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doPing(a.pc_id)} disabled={ping.isPending}>
                    <Activity className="size-3.5" />{t('actions.ping')}
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doGroups(a.pc_id)} disabled={groupsGet.isPending || groupsPut.isPending}>
                    <Users className="size-3.5" />{t('actions.groups')}
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doEffective(a.pc_id)} disabled={effective.isPending}>
                    <Settings2 className="size-3.5" />{t('actions.effective')}
                  </Button>
                </div>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>

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
