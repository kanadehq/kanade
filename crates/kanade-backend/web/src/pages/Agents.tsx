import { useQuery } from '@tanstack/react-query';
import { Loader2, Server } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { AgentRow } from '@/lib/types';

function fmtBytes(n: number | null): string {
  if (!n) return '—';
  const gb = n / 1024 ** 3;
  return `${gb.toFixed(1)} GB`;
}

function fmtTime(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

export function Agents() {
  const { data, error, isLoading } = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
  });

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />
        loading agents…
      </div>
    );
  }

  if (error) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-danger">Couldn't load agents</CardTitle>
        </CardHeader>
        <CardContent>
          <pre className="text-sm whitespace-pre-wrap text-danger">{error.message}</pre>
        </CardContent>
      </Card>
    );
  }

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
        <Badge variant="violet">{agents.length} online</Badge>
      </div>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>pc_id</TableHead>
            <TableHead>hostname</TableHead>
            <TableHead>os</TableHead>
            <TableHead>cpu</TableHead>
            <TableHead>ram</TableHead>
            <TableHead>last_inventory</TableHead>
            <TableHead>actions</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {agents.map((a) => (
            <TableRow key={a.pc_id}>
              <TableCell><code className="text-xs">{a.pc_id}</code></TableCell>
              <TableCell>{a.hostname ?? <span className="text-muted">—</span>}</TableCell>
              <TableCell>
                {a.os_name ?? ''} {a.os_version ?? ''}
                {a.os_build && <span className="text-muted text-xs ml-1">{a.os_build}</span>}
              </TableCell>
              <TableCell>
                {a.cpu_model ?? <span className="text-muted">—</span>}
                {a.cpu_cores ? <span className="text-muted text-xs ml-1">×{a.cpu_cores}</span> : null}
              </TableCell>
              <TableCell className="tabular-nums">{fmtBytes(a.ram_bytes)}</TableCell>
              <TableCell className="text-muted text-xs">{fmtTime(a.last_inventory)}</TableCell>
              <TableCell>
                <div className="flex gap-1 flex-wrap">
                  <Button variant="secondary" size="sm" disabled>ping</Button>
                  <Button variant="secondary" size="sm" disabled>groups</Button>
                  <Button variant="secondary" size="sm" disabled>effective</Button>
                </div>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      <p className="text-xs text-muted">
        Row actions wired up in the next port (Run / Config / per-agent detail are coming).
      </p>
    </div>
  );
}
