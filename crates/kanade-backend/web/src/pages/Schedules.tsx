import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Power, PowerOff, Trash2 } from 'lucide-react';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';

type ScheduleRow = {
  id: string;
  cron: string;
  job_id: string;
  enabled: boolean;
};

export function Schedules() {
  const qc = useQueryClient();
  const { data, error, isLoading } = useQuery({
    queryKey: ['schedules'],
    queryFn: () => apiFetch<ScheduleRow[]>('/api/schedules'),
  });

  const del = useMutation({
    mutationFn: (id: string) => apiFetch(`/api/schedules/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['schedules'] }),
  });

  // POST /api/schedules is an upsert, so we just re-POST the full row
  // with `enabled` flipped. The scheduler's KV watcher picks up the
  // change and registers/unregisters the cron job on the next put.
  const toggle = useMutation({
    mutationFn: (s: ScheduleRow) =>
      apiFetch('/api/schedules', {
        method: 'POST',
        body: JSON.stringify({ ...s, enabled: !s.enabled }),
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['schedules'] }),
  });

  if (isLoading) return <div className="flex items-center gap-2 text-muted"><Loader2 className="size-4 animate-spin" />loading schedules…</div>;
  if (error) return <ErrorCard title="Couldn't load schedules" error={error} />;
  const rows = data ?? [];

  if (rows.length === 0) {
    return (
      <Card>
        <CardHeader><CardTitle>No schedules yet</CardTitle></CardHeader>
        <CardContent className="text-muted">
          Use <code>kanade schedule create &lt;schedule.yaml&gt;</code> to upsert one. YAML-form
          creation in the web UI is on the backlog.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Schedules</h2>
        <Badge variant="violet">{rows.length}</Badge>
      </div>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>id</TableHead>
            <TableHead>cron</TableHead>
            <TableHead>job_id</TableHead>
            <TableHead>enabled</TableHead>
            <TableHead>actions</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((s) => (
            <TableRow key={s.id}>
              <TableCell><code className="text-xs">{s.id}</code></TableCell>
              <TableCell><code className="text-xs">{s.cron}</code></TableCell>
              <TableCell><code className="text-xs">{s.job_id}</code></TableCell>
              <TableCell>
                {s.enabled
                  ? <Badge variant="success">on</Badge>
                  : <Badge variant="danger">off</Badge>}
              </TableCell>
              <TableCell className="flex gap-2">
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={toggle.isPending}
                  onClick={() => toggle.mutate(s)}
                  title={s.enabled ? 'Disable this schedule' : 'Enable this schedule'}
                >
                  {s.enabled
                    ? <><PowerOff className="size-3.5" />disable</>
                    : <><Power className="size-3.5" />enable</>}
                </Button>
                <Button
                  variant="danger"
                  size="sm"
                  disabled={del.isPending}
                  onClick={() => {
                    if (window.confirm(`Delete schedule ${s.id}?`)) del.mutate(s.id);
                  }}
                >
                  <Trash2 className="size-3.5" />
                  delete
                </Button>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {del.error && <ErrorCard title="Delete failed" error={del.error} />}
      {toggle.error && <ErrorCard title="Toggle failed" error={toggle.error} />}
    </div>
  );
}
