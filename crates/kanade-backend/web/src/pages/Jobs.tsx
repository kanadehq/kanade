import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, ScrollText, Trash2 } from 'lucide-react';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';

type JobRow = {
  id: string;
  version: string;
  description: string | null;
  execute: {
    shell: 'powershell' | 'cmd';
    timeout: string;
    run_as?: 'system' | 'user' | 'system_gui';
    cwd?: string | null;
  };
  inventory: unknown | null;
};

export function Jobs() {
  const qc = useQueryClient();
  const { data, error, isLoading } = useQuery({
    queryKey: ['jobs'],
    queryFn: () => apiFetch<JobRow[]>('/api/jobs'),
  });

  const del = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/jobs/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['jobs'] }),
  });

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />loading jobs…
      </div>
    );
  }
  if (error) return <ErrorCard title="Couldn't load jobs" error={error} />;
  const rows = data ?? [];

  if (rows.length === 0) {
    return (
      <Card>
        <CardHeader><CardTitle>No jobs registered</CardTitle></CardHeader>
        <CardContent className="text-muted">
          Use <code>kanade job create &lt;manifest.yaml&gt;</code> to upsert one.
          Schedules reference jobs by id, so the job must exist before the
          schedule fires. YAML-form creation in the web UI is on the backlog.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Jobs</h2>
        <Badge variant="violet">{rows.length}</Badge>
      </div>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>id</TableHead>
            <TableHead>version</TableHead>
            <TableHead>shell</TableHead>
            <TableHead>run_as</TableHead>
            <TableHead>cwd</TableHead>
            <TableHead>timeout</TableHead>
            <TableHead>inventory</TableHead>
            <TableHead>description</TableHead>
            <TableHead>actions</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((j) => (
            <TableRow key={j.id}>
              <TableCell><code className="text-xs">{j.id}</code></TableCell>
              <TableCell><code className="text-xs">{j.version}</code></TableCell>
              <TableCell><code className="text-xs">{j.execute.shell}</code></TableCell>
              <TableCell><code className="text-xs">{j.execute.run_as ?? 'system'}</code></TableCell>
              <TableCell>
                {j.execute.cwd
                  ? <code className="text-xs">{j.execute.cwd}</code>
                  : <span className="text-muted text-xs">—</span>}
              </TableCell>
              <TableCell><code className="text-xs">{j.execute.timeout}</code></TableCell>
              <TableCell>
                {j.inventory
                  ? <Badge variant="violet"><ScrollText className="size-3" />probe</Badge>
                  : <span className="text-muted text-xs">—</span>}
              </TableCell>
              <TableCell className="text-xs text-muted max-w-md truncate">
                {j.description ?? '—'}
              </TableCell>
              <TableCell>
                <Button
                  variant="danger"
                  size="sm"
                  disabled={del.isPending}
                  onClick={() => {
                    if (window.confirm(`Delete job ${j.id}?\n\n(Refused with 409 if any schedule references it.)`))
                      del.mutate(j.id);
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
    </div>
  );
}
