import { useMutation, useQuery } from '@tanstack/react-query';
import { Activity, Loader2, ScrollText, Server, Settings2, Users } from 'lucide-react';
import { useState } from 'react';
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

export function Agents() {
  const { data, error, isLoading } = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
  });
  const [result, setResult] = useState<ActionResult | null>(null);

  const ping = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch<{ heartbeat: Heartbeat }>(`/api/agents/${encodeURIComponent(pcId)}/ping?wait_secs=45`, {
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
    setResult({ pc_id: pcId, action: 'groups', value: 'loading…' });
    try {
      const current = await groupsGet.mutateAsync(pcId);
      const next = window.prompt(
        `Comma-separated group names for ${pcId} (current: ${current.groups.join(', ') || '(none)'})`,
        current.groups.join(', '),
      );
      if (next === null) {
        setResult({ pc_id: pcId, action: 'groups', value: '(cancelled)' });
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
        loading agents…
      </div>
    );
  }
  if (error) return <ErrorCard title="Couldn't load agents" error={error} />;
  const agents = data ?? [];

  if (agents.length === 0) {
    return (
      <Card>
        <CardHeader className="items-center text-center">
          <Server className="size-10 text-muted" />
          <CardTitle>No agents yet</CardTitle>
        </CardHeader>
        <CardContent className="text-center text-muted">
          Boot a kanade-agent service and it'll show up here within one heartbeat.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Agents</h2>
        <Badge variant="violet">{agents.length} known</Badge>
      </div>
      <p className="text-xs text-muted">
        Baseline liveness only — open <Link to="/inventory" className="underline">Inventory</Link>{' '}
        (or click <strong>facts</strong>) for richer per-host details collected by operator-defined probes.
      </p>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>pc_id</TableHead>
            <TableHead>hostname</TableHead>
            <TableHead>os</TableHead>
            <TableHead>agent</TableHead>
            <TableHead>last heartbeat</TableHead>
            <TableHead>actions</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {agents.map((a) => (
            <TableRow key={a.pc_id}>
              <TableCell><code className="text-xs">{a.pc_id}</code></TableCell>
              <TableCell>{a.hostname ?? <span className="text-muted">—</span>}</TableCell>
              <TableCell className="text-muted text-xs">{a.os_family ?? '—'}</TableCell>
              <TableCell className="text-muted text-xs">{a.agent_version ?? '—'}</TableCell>
              <TableCell className="text-muted text-xs">{fmtIsoLocal(a.last_heartbeat)}</TableCell>
              <TableCell>
                <div className="flex gap-1 flex-wrap">
                  <Button variant="secondary" size="sm" asChild>
                    <Link to={`/inventory?pc=${encodeURIComponent(a.pc_id)}`}>
                      <ScrollText className="size-3.5" />facts
                    </Link>
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doPing(a.pc_id)} disabled={ping.isPending}>
                    <Activity className="size-3.5" />ping
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doGroups(a.pc_id)} disabled={groupsGet.isPending || groupsPut.isPending}>
                    <Users className="size-3.5" />groups
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => doEffective(a.pc_id)} disabled={effective.isPending}>
                    <Settings2 className="size-3.5" />effective
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
