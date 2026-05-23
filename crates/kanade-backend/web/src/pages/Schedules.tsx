import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ChevronDown, FilePlus2, Loader2, Pencil, Power, PowerOff, Trash2, Zap } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { type EditorMode, YamlEditorDialog } from '@/components/YamlEditorDialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, formatError } from '@/lib/api';

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

function summariseTarget(target: ScheduleRow['target'], allLabel: string): string {
  if (target.all) return allLabel;
  const parts: string[] = [];
  if (target.groups.length) parts.push(`groups: ${target.groups.join(', ')}`);
  if (target.pcs.length) parts.push(`pcs: ${target.pcs.join(', ')}`);
  return parts.join(' · ') || '—';
}

export function Schedules() {
  const { t } = useTranslation('schedules');
  const qc = useQueryClient();
  const confirm = useConfirm();
  const { data, error, isLoading } = useQuery({
    queryKey: ['schedules'],
    queryFn: () => apiFetch<ScheduleRow[]>('/api/schedules'),
  });

  const del = useMutation({
    mutationFn: (id: string) => apiFetch(`/api/schedules/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['schedules'] });
      toast.success(t('toast.deleted', { id }));
    },
    onError: (e) => toast.error(t('toast.deleteFailed', { error: formatError(e) })),
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

  // v0.32 / PR-B: shared Monaco-backed YAML editor — same state shape
  // and behaviour as the Jobs page.
  const [editor, setEditor] = useState<EditorMode | null>(null);
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
    onSuccess: (_d, { id, cascade }) => {
      qc.invalidateQueries({ queryKey: ['schedules'] });
      toast.success(cascade ? t('toast.hardDisabled', { id }) : t('toast.softDisabled', { id }));
    },
    onError: (e) => toast.error(t('toast.disableFailed', { error: formatError(e) })),
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
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['schedules'] });
      toast.success(t('toast.enabled', { id }));
    },
    onError: (e) => toast.error(t('toast.enableFailed', { error: formatError(e) })),
  });

  if (isLoading) return <div className="flex items-center gap-2 text-muted"><Loader2 className="size-4 animate-spin" />{t('loading')}</div>;
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
                {t('newSchedule')}
              </Button>
            </div>
          </CardHeader>
          <CardContent className="text-muted">
            <Trans
              ns="schedules"
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
            kind="schedule"
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
            title={t('newScheduleTitle')}
          >
            <FilePlus2 className="size-3.5" />
            {t('newSchedule')}
          </Button>
          <Badge variant="violet">{rows.length}</Badge>
        </div>
      </div>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>{t('columns.id')}</TableHead>
            <TableHead>{t('columns.cron')}</TableHead>
            <TableHead>{t('columns.jobId')}</TableHead>
            <TableHead>{t('columns.target')}</TableHead>
            <TableHead>{t('columns.runsOn')}</TableHead>
            <TableHead>{t('columns.mode')}</TableHead>
            <TableHead>{t('columns.cooldown')}</TableHead>
            <TableHead>{t('columns.deadline')}</TableHead>
            <TableHead>{t('columns.autoOff')}</TableHead>
            <TableHead>{t('columns.jitter')}</TableHead>
            <TableHead>{t('columns.rollout')}</TableHead>
            <TableHead>{t('columns.enabled')}</TableHead>
            <TableHead>{t('columns.actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((s) => (
            <TableRow key={s.id}>
              <TableCell><code className="text-xs">{s.id}</code></TableCell>
              <TableCell><code className="text-xs">{s.cron}</code></TableCell>
              <TableCell><code className="text-xs">{s.job_id}</code></TableCell>
              <TableCell className="text-xs">{summariseTarget(s.target, t('target.all'))}</TableCell>
              <TableCell><code className="text-xs">{s.runs_on}</code></TableCell>
              <TableCell><code className="text-xs">{s.mode}</code></TableCell>
              <TableCell><code className="text-xs">{s.cooldown ?? '—'}</code></TableCell>
              <TableCell><code className="text-xs">{s.starting_deadline ?? '—'}</code></TableCell>
              <TableCell className="text-xs">
                {s.auto_disable_when_done ? t('autoOff.yes') : <span className="text-muted">—</span>}
              </TableCell>
              <TableCell><code className="text-xs">{s.jitter ?? '—'}</code></TableCell>
              <TableCell className="text-xs">
                {s.rollout
                  ? t('rollout', { count: s.rollout.waves.length })
                  : <span className="text-muted">—</span>}
              </TableCell>
              <TableCell>
                {s.enabled
                  ? <Badge variant="success">{t('status.on')}</Badge>
                  : <Badge variant="danger">{t('status.off')}</Badge>}
              </TableCell>
              <TableCell className="flex flex-wrap gap-2">
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => setEditor({ type: 'edit', id: s.id })}
                  title={t('actions.editTitle')}
                  aria-label={t('actions.editAria', { id: s.id })}
                >
                  <Pencil className="size-3.5" />
                </Button>
                {s.enabled ? (
                  // v0.33 — merged the two "disable" buttons (Soft +
                  // Hard cascade) into one split-button dropdown so the
                  // Actions column stops wrapping to two rows. The
                  // dropdown puts both choices on screen at the same
                  // time with a one-line explainer, which reads more
                  // safely than two adjacent buttons where the
                  // operator might mistake the destructive variant for
                  // the soft one.
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                      <Button
                        variant="secondary"
                        size="sm"
                        disabled={pendingDisable.has(s.id)}
                        title={t('actions.disableMenuTitle')}
                        aria-label={t('actions.disableMenuAria', { id: s.id })}
                      >
                        <PowerOff className="size-3.5" />
                        <ChevronDown className="size-3" />
                      </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end">
                      <DropdownMenuItem
                        onSelect={() => disable.mutate({ id: s.id, cascade: false })}
                      >
                        <PowerOff className="size-4 mt-0.5 shrink-0" />
                        <div className="flex flex-col gap-0.5">
                          <span>{t('actions.softDisable')}</span>
                          <span className="text-xs text-muted">
                            {t('actions.softDisableHint')}
                          </span>
                        </div>
                      </DropdownMenuItem>
                      <DropdownMenuSeparator />
                      <DropdownMenuItem
                        variant="danger"
                        onSelect={async () => {
                          const ok = await confirm({
                            title: t('confirm.hardDisableTitle', { id: s.id }),
                            description: t('confirm.hardDisableDescription', { id: s.id, jobId: s.job_id }),
                            confirmLabel: t('confirm.hardDisableLabel'),
                            danger: true,
                          });
                          if (ok) disable.mutate({ id: s.id, cascade: true });
                        }}
                      >
                        <Zap className="size-4 mt-0.5 shrink-0" />
                        <div className="flex flex-col gap-0.5">
                          <span>{t('actions.hardDisable')}</span>
                          <span className="text-xs text-muted">
                            {t('actions.hardDisableHint')}
                          </span>
                        </div>
                      </DropdownMenuItem>
                    </DropdownMenuContent>
                  </DropdownMenu>
                ) : (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={pendingEnable.has(s.id)}
                    onClick={() => enable.mutate(s.id)}
                    title={t('actions.enableTitle')}
                    aria-label={t('actions.enableAria', { id: s.id })}
                  >
                    <Power className="size-3.5" />
                  </Button>
                )}
                <Button
                  variant="danger"
                  size="sm"
                  disabled={del.isPending}
                  onClick={async () => {
                    const ok = await confirm({
                      title: t('confirm.deleteTitle', { id: s.id }),
                      description: t('confirm.deleteDescription'),
                      confirmLabel: t('confirm.deleteLabel'),
                      danger: true,
                    });
                    if (ok) del.mutate(s.id);
                  }}
                  title={t('actions.deleteTitle')}
                  aria-label={t('actions.deleteAria', { id: s.id })}
                >
                  <Trash2 className="size-3.5" />
                </Button>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {del.error && <ErrorCard title={t('errors.deleteFailed')} error={del.error} />}
      {disable.error && <ErrorCard title={t('errors.disableFailed')} error={disable.error} />}
      {enable.error && <ErrorCard title={t('errors.enableFailed')} error={enable.error} />}
      {editor !== null && (
        <YamlEditorDialog
          open
          onOpenChange={(next) => {
            if (!next) setEditor(null);
          }}
          kind="schedule"
          mode={editor}
        />
      )}
    </div>
  );
}
