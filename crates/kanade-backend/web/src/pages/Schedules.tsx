import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Power, PowerOff, Trash2, Zap } from 'lucide-react';
import { useState } from 'react';

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
  target: { all: boolean; groups: string[]; pcs: string[] };
  rollout: { waves: { group: string; delay: string }[] } | null;
  jitter: string | null;
  mode: 'every_tick' | 'once_per_pc' | 'once_per_target';
  cooldown: string | null;
  auto_disable_when_done: boolean;
  starting_deadline: string | null;
  runs_on: 'backend' | 'agent';
  enabled: boolean;
};

function summariseTarget(t: ScheduleRow['target']): string {
  if (t.all) return 'all';
  const parts: string[] = [];
  if (t.groups.length) parts.push(`groups: ${t.groups.join(', ')}`);
  if (t.pcs.length) parts.push(`pcs: ${t.pcs.join(', ')}`);
  return parts.join(' · ') || '—';
}

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

  // v0.27 (SPEC §2.6.4 (c)): disable goes through the dedicated
  // endpoint that can also cascade-revoke the referenced Job.
  // ?cascade=true = "hard disable" — stops the cron AND writes
  // script_status.{job_id} = REVOKED so any in-flight Command gets
  // skipped at agent fire time. ?cascade=false (default) = "soft
  // disable" — just stops the cron, in-flight Commands run.
  //
  // Round 2 review (CodeRabbit #38): per-row pending tracked via a
  // Set<string> so concurrent disable/enable clicks across rows
  // don't grey each other out — `mutation.variables` is a single
  // value, useless for per-row gating.
  const [pendingDisable, setPendingDisable] = useState<Set<string>>(new Set());
  const [pendingEnable, setPendingEnable] = useState<Set<string>>(new Set());
  const disable = useMutation({
    mutationFn: ({ id, cascade }: { id: string; cascade: boolean }) =>
      apiFetch(`/api/schedules/${encodeURIComponent(id)}/disable?cascade=${cascade}`, {
        method: 'POST',
      }),
    onMutate: ({ id }) => {
      setPendingDisable((prev) => new Set(prev).add(id));
    },
    onSettled: (_d, _e, { id }) => {
      setPendingDisable((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ['schedules'] }),
  });
  // v0.27 (gemini #38 review): symmetrical /enable endpoint so we
  // don't clobber concurrent edits with a full row re-POST. Backend
  // uses kv.entry().revision + update() the same way disable does.
  const enable = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/schedules/${encodeURIComponent(id)}/enable`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingEnable((prev) => new Set(prev).add(id));
    },
    onSettled: (_d, _e, id) => {
      setPendingEnable((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
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
            <TableHead>target</TableHead>
            <TableHead>runs_on</TableHead>
            <TableHead>mode</TableHead>
            <TableHead>cooldown</TableHead>
            <TableHead>deadline</TableHead>
            <TableHead>auto-off</TableHead>
            <TableHead>jitter</TableHead>
            <TableHead>rollout</TableHead>
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
              <TableCell className="text-xs">{summariseTarget(s.target)}</TableCell>
              <TableCell><code className="text-xs">{s.runs_on}</code></TableCell>
              <TableCell><code className="text-xs">{s.mode}</code></TableCell>
              <TableCell><code className="text-xs">{s.cooldown ?? '—'}</code></TableCell>
              <TableCell><code className="text-xs">{s.starting_deadline ?? '—'}</code></TableCell>
              <TableCell className="text-xs">
                {s.auto_disable_when_done ? 'yes' : <span className="text-muted">—</span>}
              </TableCell>
              <TableCell><code className="text-xs">{s.jitter ?? '—'}</code></TableCell>
              <TableCell className="text-xs">
                {s.rollout
                  ? `${s.rollout.waves.length} wave(s)`
                  : <span className="text-muted">—</span>}
              </TableCell>
              <TableCell>
                {s.enabled
                  ? <Badge variant="success">on</Badge>
                  : <Badge variant="danger">off</Badge>}
              </TableCell>
              <TableCell className="flex flex-wrap gap-2">
                {s.enabled ? (
                  <>
                    <Button
                      variant="secondary"
                      size="sm"
                      disabled={pendingDisable.has(s.id)}
                      onClick={() => disable.mutate({ id: s.id, cascade: false })}
                      title="Soft disable — cron stops on next tick. In-flight Commands run."
                    >
                      <PowerOff className="size-3.5" />
                      disable
                    </Button>
                    <Button
                      variant="danger"
                      size="sm"
                      disabled={pendingDisable.has(s.id)}
                      onClick={() => {
                        if (
                          window.confirm(
                            `Hard-disable schedule ${s.id}?\n\n` +
                              `(1) cron stops on next tick (same as soft disable)\n` +
                              `(2) script_status.${s.job_id} → REVOKED — any Command already in flight for this job will be skipped by the agent's Layer 2 check.\n\n` +
                              `Use this when an active rollout needs to stop NOW.`,
                          )
                        )
                          disable.mutate({ id: s.id, cascade: true });
                      }}
                      title="Hard disable — also revoke the underlying Job so in-flight Commands skip."
                    >
                      <Zap className="size-3.5" />
                      disable + cascade
                    </Button>
                  </>
                ) : (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={pendingEnable.has(s.id)}
                    onClick={() => enable.mutate(s.id)}
                    title="Re-enable this schedule"
                  >
                    <Power className="size-3.5" />
                    enable
                  </Button>
                )}
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
      {disable.error && <ErrorCard title="Disable failed" error={disable.error} />}
      {enable.error && <ErrorCard title="Enable failed" error={enable.error} />}
    </div>
  );
}
