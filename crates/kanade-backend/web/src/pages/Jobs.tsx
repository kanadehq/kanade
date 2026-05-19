import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Ban, CircleCheck, Loader2, ScrollText, Trash2 } from 'lucide-react';
import { useState } from 'react';

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

  // v0.27: surface Layer 2 revoke / unrevoke as per-row buttons so
  // operators don't have to drop to the CLI for a one-script gate
  // flip. Backend endpoint (POST /api/scripts/{cmd_id}/revoke) just
  // writes the script_status KV, which the agent's handle_command
  // reads at fire time. Idempotent on the server side — re-clicking
  // is a no-op put.
  //
  // Round 2 review (CodeRabbit #38): a single shared `useMutation`
  // overwrites `.variables` with every new invocation, so the
  // previous `disabled={isPending && variables === id}` flickered
  // back to enabled the moment a second row was clicked while the
  // first was still inflight. Track pending IDs in a `Set<string>`
  // updated in onMutate / onSettled — true per-row scoping that
  // survives concurrent clicks.
  const [pendingRevoke, setPendingRevoke] = useState<Set<string>>(new Set());
  const [pendingUnrevoke, setPendingUnrevoke] = useState<Set<string>>(new Set());
  const revoke = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/scripts/${encodeURIComponent(id)}/revoke`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingRevoke((prev) => new Set(prev).add(id));
    },
    onSettled: (_d, _e, id) => {
      setPendingRevoke((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
  });
  const unrevoke = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/scripts/${encodeURIComponent(id)}/unrevoke`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingUnrevoke((prev) => new Set(prev).add(id));
    },
    onSettled: (_d, _e, id) => {
      setPendingUnrevoke((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
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
              <TableCell className="flex flex-wrap gap-2">
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={pendingRevoke.has(j.id)}
                  onClick={() => {
                    if (
                      window.confirm(
                        `Revoke ${j.id}?\n\n` +
                          `Any in-flight Command for this manifest will be skipped on receipt by the agent's Layer 2 check (SPEC §2.6.2).\n\n` +
                          `Reversible with the unrevoke button.`,
                      )
                    )
                      revoke.mutate(j.id);
                  }}
                  title="Flip script_status to REVOKED so in-flight Commands skip on receipt"
                >
                  <Ban className="size-3.5" />
                  revoke
                </Button>
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={pendingUnrevoke.has(j.id)}
                  onClick={() => unrevoke.mutate(j.id)}
                  title="Flip script_status back to ACTIVE"
                >
                  <CircleCheck className="size-3.5" />
                  unrevoke
                </Button>
                <Button
                  variant="danger"
                  size="sm"
                  disabled={del.isPending}
                  onClick={() => {
                    if (
                      window.confirm(
                        `Delete job ${j.id}?\n\n` +
                          `This also writes script_status.${j.id} = REVOKED so any in-flight Command for this manifest is skipped on receipt (v0.27 cascade).\n\n` +
                          `Refused with 409 if any schedule references it.`,
                      )
                    )
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
      {revoke.error && <ErrorCard title="Revoke failed" error={revoke.error} />}
      {unrevoke.error && <ErrorCard title="Unrevoke failed" error={unrevoke.error} />}
    </div>
  );
}
