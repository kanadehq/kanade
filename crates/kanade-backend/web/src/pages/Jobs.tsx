import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Ban,
  CircleCheck,
  FilePlus2,
  Hourglass,
  Loader2,
  Pencil,
  Play,
  ScrollText,
  Skull,
  Trash2,
} from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { type EditorMode, YamlEditorDialog } from '@/components/YamlEditorDialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, formatError } from '@/lib/api';

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
  const { t } = useTranslation('jobs');
  const qc = useQueryClient();
  // v0.34.1 (#117) wired in ConfirmDialogProvider but Jobs.tsx
  // only added the import — the hook call itself was missing, so
  // `confirm(...)` in the kill / revoke / delete handlers below
  // was resolving against `window.confirm` (which takes a string
  // and ignored the ConfirmOptions object). Adding the hook here
  // restores the intended Promise-based modal flow.
  const confirm = useConfirm();
  const { data, error, isLoading } = useQuery({
    queryKey: ['jobs'],
    queryFn: () => apiFetch<JobRow[]>('/api/jobs'),
  });

  const del = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/jobs/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['jobs'] });
      qc.invalidateQueries({ queryKey: ['scripts-status'] });
      toast.success(t('toast.deleteSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.deleteFailure', { error: formatError(e) })),
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

  // v0.32 / PR-B: Monaco-backed YAML editor for add / edit. Null when
  // the modal is closed; a fresh mode object opens it on the right
  // shape ({ type: 'create' } for the "New job" button, { type:
  // 'edit', id } for the per-row Edit button).
  const [editor, setEditor] = useState<EditorMode | null>(null);
  const revoke = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/scripts/${encodeURIComponent(id)}/revoke`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingRevoke((prev) => new Set(prev).add(id));
    },
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['scripts-status'] });
      toast.success(t('toast.revokeSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.revokeFailure', { error: formatError(e) })),
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
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['scripts-status'] });
      toast.success(t('toast.unrevokeSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.unrevokeFailure', { error: formatError(e) })),
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
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['jobs'] });
      toast.success(t('toast.killSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.killFailure', { error: formatError(e) })),
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
        <Loader2 className="size-4 animate-spin" />{t('loading')}
      </div>
    );
  }
  if (error) return <ErrorCard title={t('errorTitle')} error={error} />;
  const rows = data ?? [];

  if (rows.length === 0) {
    return (
      <>
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between">
              <CardTitle>{t('empty.title')}</CardTitle>
              <Button
                variant="default"
                size="sm"
                onClick={() => setEditor({ type: 'create' })}
              >
                <FilePlus2 className="size-3.5" />
                {t('newJob')}
              </Button>
            </div>
          </CardHeader>
          <CardContent className="text-muted">
            <Trans
              ns="jobs"
              i18nKey="empty.body"
              components={{
                code: <code />,
                strong: <strong />,
              }}
            />
          </CardContent>
        </Card>
        {editor !== null && (
          <YamlEditorDialog
            open
            onOpenChange={(next) => {
              if (!next) setEditor(null);
            }}
            kind="manifest"
            mode={editor}
          />
        )}
      </>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <div className="flex items-center gap-2">
          <Button
            variant="default"
            size="sm"
            onClick={() => setEditor({ type: 'create' })}
            title={t('newJobTitle')}
          >
            <FilePlus2 className="size-3.5" />
            {t('newJob')}
          </Button>
          <Badge variant="violet">{rows.length}</Badge>
        </div>
      </div>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>{t('columns.id')}</TableHead>
            <TableHead>{t('columns.version')}</TableHead>
            <TableHead>{t('columns.status')}</TableHead>
            <TableHead>{t('columns.live')}</TableHead>
            <TableHead>{t('columns.shell')}</TableHead>
            <TableHead>{t('columns.runAs')}</TableHead>
            <TableHead>{t('columns.cwd')}</TableHead>
            <TableHead>{t('columns.timeout')}</TableHead>
            <TableHead>{t('columns.inventory')}</TableHead>
            <TableHead>{t('columns.description')}</TableHead>
            <TableHead>{t('columns.actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((j) => (
            <TableRow key={j.id}>
              <TableCell><code className="text-xs">{j.id}</code></TableCell>
              <TableCell><code className="text-xs">{j.version}</code></TableCell>
              <TableCell>
                {isRevoked(j.id) ? (
                  <Badge variant="danger">{t('status.revoked')}</Badge>
                ) : (
                  <Badge variant="success">{t('status.active')}</Badge>
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
                        title={t('live.runningTitle')}
                        className="inline-flex items-center gap-1 px-1.5"
                      >
                        <Play className="size-3" />
                        {j.live.running}
                      </Badge>
                    )}
                    {j.live.pending > 0 && (
                      <Badge
                        variant="amber"
                        title={t('live.pendingTitle')}
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
              {/* Backslash-separated Windows paths have no break
                  opportunities, so an unconstrained cell claims the
                  full path width and forces the whole table into
                  horizontal scroll. Cap + truncate (same pattern as
                  the description cell below), full path in the
                  tooltip. */}
              <TableCell className="max-w-48 truncate" title={j.execute.cwd || undefined}>
                {j.execute.cwd
                  ? <code className="text-xs">{j.execute.cwd}</code>
                  : <span className="text-muted text-xs">—</span>}
              </TableCell>
              <TableCell><code className="text-xs">{j.execute.timeout}</code></TableCell>
              <TableCell>
                {j.inventory
                  ? <Badge variant="violet"><ScrollText className="size-3" />{t('inventoryProbe')}</Badge>
                  : <span className="text-muted text-xs">—</span>}
              </TableCell>
              {/* `truncate` implies nowrap, so the old `max-w-md`
                  made every long description demand a hard 28rem of
                  min-content — pushing the table past the viewport
                  even maximized. `w-full max-w-0` instead lets this
                  column soak up whatever space is left after the
                  fixed-content columns, truncating to fit. */}
              <TableCell
                className="text-xs text-muted w-full max-w-0 truncate"
                title={j.description || undefined}
              >
                {j.description || '—'}
              </TableCell>
              <TableCell className="flex flex-nowrap gap-2">
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
                      onClick={async () => {
                        const ok = await confirm({
                          title: t('confirm.killTitle', { id: j.id }),
                          description: t('confirm.killDescription', {
                            count: inflight,
                            running: j.live.running,
                            pending: j.live.pending,
                          }),
                          confirmLabel: t('confirm.killLabel'),
                          danger: true,
                        });
                        if (ok) kill.mutate(j.id);
                      }}
                      title={t('actions.killTitle', { count: inflight })}
                      aria-label={t('actions.killAria', { id: j.id })}
                    >
                      <Skull className="size-3.5" />
                    </Button>
                  ) : null;
                })()}
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => setEditor({ type: 'edit', id: j.id })}
                  title={t('actions.editTitle')}
                  aria-label={t('actions.editAria', { id: j.id })}
                >
                  <Pencil className="size-3.5" />
                </Button>
                {!isRevoked(j.id) && (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={pendingRevoke.has(j.id)}
                    onClick={async () => {
                      const ok = await confirm({
                        title: t('confirm.revokeTitle', { id: j.id }),
                        description: t('confirm.revokeDescription'),
                        confirmLabel: t('confirm.revokeLabel'),
                        danger: true,
                      });
                      if (ok) revoke.mutate(j.id);
                    }}
                    title={t('actions.revokeTitle')}
                    aria-label={t('actions.revokeAria', { id: j.id })}
                  >
                    <Ban className="size-3.5" />
                  </Button>
                )}
                {isRevoked(j.id) && (
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={pendingUnrevoke.has(j.id)}
                  onClick={() => unrevoke.mutate(j.id)}
                  title={t('actions.unrevokeTitle')}
                  aria-label={t('actions.unrevokeAria', { id: j.id })}
                >
                  <CircleCheck className="size-3.5" />
                </Button>
                )}
                <Button
                  variant="danger"
                  size="sm"
                  disabled={del.isPending}
                  onClick={async () => {
                    const ok = await confirm({
                      title: t('confirm.deleteTitle', { id: j.id }),
                      description: t('confirm.deleteDescription'),
                      confirmLabel: t('confirm.deleteLabel'),
                      danger: true,
                    });
                    if (ok) del.mutate(j.id);
                  }}
                  title={t('actions.deleteTitle')}
                  aria-label={t('actions.deleteAria', { id: j.id })}
                >
                  <Trash2 className="size-3.5" />
                </Button>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {del.error && <ErrorCard title={t('errors.deleteTitle')} error={del.error} />}
      {revoke.error && <ErrorCard title={t('errors.revokeTitle')} error={revoke.error} />}
      {unrevoke.error && <ErrorCard title={t('errors.unrevokeTitle')} error={unrevoke.error} />}
      {kill.error && <ErrorCard title={t('errors.killTitle')} error={kill.error} />}
      {editor !== null && (
        <YamlEditorDialog
          open
          onOpenChange={(next) => {
            if (!next) setEditor(null);
          }}
          kind="manifest"
          mode={editor}
        />
      )}
    </div>
  );
}
