import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Ban, CircleCheck, Hourglass, Loader2, Play, ScrollText, Skull, Trash2 } from 'lucide-react';
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
  /** v0.30 / PR γ: in-flight counters joined onto each row by the
   *  backend so the Jobs page can show "is anything running right
   *  now" — drives the per-row live chip + kill button enable
   *  state. Zeros when no execution rows exist for this cmd. */
  live: {
    running: number;
    pending: number;
  };
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
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['jobs'] });
      qc.invalidateQueries({ queryKey: ['scripts-status'] });
    },
  });

  // v0.27.x: surface the script_status KV (per cmd_id ACTIVE/REVOKED)
  // alongside the job catalog so operators can SEE whether a revoke
  // landed instead of guessing. Empty map when the bucket is missing
  // — silently degrades to "everything is ACTIVE" which is the safe
  // pre-revoke default.
  const statusQuery = useQuery({
    queryKey: ['scripts-status'],
    queryFn: () => apiFetch<Record<string, string>>('/api/scripts/status'),
  });
  const statusMap = statusQuery.data ?? {};
  function isRevoked(id: string): boolean {
    return statusMap[id] === 'REVOKED';
  }

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
  const [pendingKill, setPendingKill] = useState<Set<string>>(new Set());
  const revoke = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/scripts/${encodeURIComponent(id)}/revoke`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingRevoke((prev) => new Set(prev).add(id));
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ['scripts-status'] }),
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
    onSuccess: () => qc.invalidateQueries({ queryKey: ['scripts-status'] }),
    onSettled: (_d, _e, id) => {
      setPendingUnrevoke((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
  });

  // v0.30 / PR γ: Layer 3 kill from the Jobs page. Distinct from
  // revoke (Layer 2) — kill stops the currently-running child
  // process for every in-flight exec of this cmd, but does NOT
  // prevent the next schedule tick from firing another fresh exec.
  // For "stop this job entirely", the operator clicks revoke
  // alongside (the confirm dialog mentions this so they don't
  // misread the scope). The backend's
  // `POST /api/jobs/{cmd_id}/kill` route (v0.29) does the
  // exec_id fan-out — pre-v0.29 it published `kill.{cmd_id}` to
  // an empty subject, which was a silent no-op.
  const kill = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/jobs/${encodeURIComponent(id)}/kill`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingKill((prev) => new Set(prev).add(id));
    },
    // Refresh /api/jobs so the live chip recomputes once results
    // start landing post-kill (kills land as ExecResult exit_code
    // -1 → projector flips status from running → completed).
    onSuccess: () => qc.invalidateQueries({ queryKey: ['jobs'] }),
    onSettled: (_d, _e, id) => {
      setPendingKill((prev) => {
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
            <TableHead>status</TableHead>
            <TableHead>live</TableHead>
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
              <TableCell>
                {isRevoked(j.id) ? (
                  <Badge variant="danger">revoked</Badge>
                ) : (
                  <Badge variant="success">active</Badge>
                )}
              </TableCell>
              <TableCell>
                {/* v0.30 follow-up: compact icon + count chips so
                    both running + pending fit in a narrow column
                    without wrapping to two lines. Tooltips carry
                    the full semantics. Stale `pending` rows (= fire
                    whose ExecResult never landed within 1 h) flip
                    to `expired` via the backend cleanup task and
                    drop out of this chip automatically. */}
                {j.live.running > 0 || j.live.pending > 0 ? (
                  <div className="flex gap-1.5 items-center">
                    {j.live.running > 0 && (
                      <Badge
                        variant="violet"
                        title="Running: at least one PC has reported back, more still in flight"
                        className="inline-flex items-center gap-1 px-1.5"
                      >
                        <Play className="size-3" />
                        {j.live.running}
                      </Badge>
                    )}
                    {j.live.pending > 0 && (
                      <Badge
                        variant="amber"
                        title="Pending: fan-out published, no results back yet (auto-expires after 1 h)"
                        className="inline-flex items-center gap-1 px-1.5"
                      >
                        <Hourglass className="size-3" />
                        {j.live.pending}
                      </Badge>
                    )}
                  </div>
                ) : (
                  <span className="text-muted text-xs">—</span>
                )}
              </TableCell>
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
                {/* v0.30 follow-up: render each action ONLY when
                    actionable for the current row state. The old
                    "always render, disable when N/A" layout left
                    4 buttons stacked 2×2 for every row regardless
                    of whether unrevoke or kill applied — visually
                    busy and hard to scan. Now:
                      * kill: shown only when something is in flight
                      * revoke: shown only when active
                      * unrevoke: shown only when revoked
                      * delete: always (it's the last-resort op) */}
                {(() => {
                  const inflight = j.live.running + j.live.pending;
                  return inflight > 0 ? (
                    <Button
                      variant="danger"
                      size="sm"
                      disabled={pendingKill.has(j.id)}
                      onClick={() => {
                        if (
                          window.confirm(
                            `Kill all in-flight runs of ${j.id}?\n\n` +
                              `${inflight} run${inflight === 1 ? '' : 's'} currently in flight ` +
                              `(running: ${j.live.running}, pending: ${j.live.pending}). ` +
                              `Each agent will terminate its child process and report back with exit_code -1.\n\n` +
                              `Note: this does NOT block the next schedule tick from firing a fresh run — ` +
                              `if you want to stop new fires too, click "revoke" alongside.`,
                          )
                        )
                          kill.mutate(j.id);
                      }}
                      title={`Terminate ${inflight} in-flight run${inflight === 1 ? '' : 's'}`}
                    >
                      <Skull className="size-3.5" />
                      kill
                    </Button>
                  ) : null;
                })()}
                {!isRevoked(j.id) && (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={pendingRevoke.has(j.id)}
                    onClick={() => {
                      if (
                        window.confirm(
                          `Revoke ${j.id}?\n\n` +
                            `Blocks the script from running on agents. Any pending or in-flight run for this job will be skipped instead of executed, and new fires will refuse to run until you unrevoke it.\n\n` +
                            `Reversible — click "unrevoke" to undo.`,
                        )
                      )
                        revoke.mutate(j.id);
                    }}
                    title="Block this script from running"
                  >
                    <Ban className="size-3.5" />
                    revoke
                  </Button>
                )}
                {isRevoked(j.id) && (
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={pendingUnrevoke.has(j.id)}
                  onClick={() => unrevoke.mutate(j.id)}
                  title="Allow this script to run again"
                >
                  <CircleCheck className="size-3.5" />
                  unrevoke
                </Button>
                )}
                <Button
                  variant="danger"
                  size="sm"
                  disabled={del.isPending}
                  onClick={() => {
                    if (
                      window.confirm(
                        `Delete job ${j.id}?\n\n` +
                          `Removes the script from the catalog. Any pending or in-flight run for this job will also be blocked (auto-revoke).\n\n` +
                          `Refused if any schedule still references this job — delete the schedule first.`,
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
      {kill.error && <ErrorCard title="Kill failed" error={kill.error} />}
    </div>
  );
}
