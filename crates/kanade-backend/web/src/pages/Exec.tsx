import { useMutation, useQuery } from '@tanstack/react-query';
import { AlertTriangle, Loader2, Send } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { GroupPicker } from '@/components/GroupPicker';
import { PcPicker } from '@/components/PcPicker';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { apiFetch } from '@/lib/api';
import { useAuth } from '@/lib/auth';

type JobRow = { id: string; version: string; description: string | null };

type ExecResponse = {
  exec_id: string;
  job_id: string;
  version: string;
  target_count: number;
  subjects: string[];
};

type TargetMode = 'all' | 'groups' | 'pcs';

type FanoutPlan = {
  target: { all: boolean; groups: string[]; pcs: string[] };
  jitter?: string;
};

export function Exec() {
  const { t } = useTranslation('exec');
  const { hasRole } = useAuth();
  const confirm = useConfirm();
  const canOperate = hasRole('operator');
  // Deep-link preselect: the Jobs page links here as
  // `/exec?job_id=<id>`, so seed the picker from the query once on
  // mount. Lazy initializer (not useEffect) so it's a plain default —
  // the operator is then free to change the selection, and we never
  // clobber that on rerender. Targets + confirm still happen here.
  const [searchParams] = useSearchParams();
  const [jobId, setJobId] = useState(() => searchParams.get('job_id') ?? '');
  // Default to 'pcs' (not 'all'): firing a job at every registered
  // agent should be a deliberate, explicit choice — never the state
  // the form lands in. With 'pcs' selected and no PCs picked,
  // `targetReady` stays false so the submit button is disabled until
  // the operator names targets.
  const [mode, setMode] = useState<TargetMode>('pcs');
  const [groups, setGroups] = useState<string[]>([]);
  const [pcs, setPcs] = useState<string[]>([]);
  const [jitter, setJitter] = useState('');

  const jobsQ = useQuery({
    queryKey: ['jobs'],
    queryFn: () => apiFetch<JobRow[]>('/api/jobs'),
  });

  const mut = useMutation({
    mutationFn: ({ id, plan }: { id: string; plan: FanoutPlan }) =>
      apiFetch<ExecResponse>(`/api/exec/${encodeURIComponent(id)}`, {
        method: 'POST',
        body: JSON.stringify(plan),
      }),
  });

  const jobs = jobsQ.data ?? [];

  const onFire = async () => {
    const targetGroups = mode === 'groups' ? groups : [];
    const targetPcs = mode === 'pcs' ? pcs : [];
    const plan: FanoutPlan = {
      target: {
        all: mode === 'all',
        groups: targetGroups,
        pcs: targetPcs,
      },
    };
    if (jitter.trim()) plan.jitter = jitter.trim();

    // Exec is fire-and-forget — once a job fans out there's no undo,
    // so confirm the blast radius first. The 'all' case gets the
    // danger styling (red confirm, Cancel auto-focused) since it hits
    // every registered agent.
    const description =
      mode === 'all'
        ? t('confirm.allWarning')
        : mode === 'groups'
          ? t('confirm.targetGroups', { groups: targetGroups.join(', ') })
          : t('confirm.targetPcs', { pcs: targetPcs.join(', ') });
    const ok = await confirm({
      title: t('confirm.title', { jobId }),
      description,
      confirmLabel: mode === 'all' ? t('confirm.confirmLabelAll') : t('confirm.confirmLabel'),
      cancelLabel: t('confirm.cancelLabel'),
      danger: mode === 'all',
    });
    if (!ok) return;

    mut.mutate({ id: jobId, plan });
  };

  const targetReady =
    mode === 'all'
      || (mode === 'groups' && groups.length > 0)
      || (mode === 'pcs' && pcs.length > 0);

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>{t('title')}</CardTitle>
          <CardDescription>
            <Trans
              ns="exec"
              i18nKey="description"
              components={{ code: <code /> }}
            />
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {jobsQ.isLoading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="size-4 animate-spin" />{t('loadingJobs')}
            </div>
          ) : jobsQ.error ? (
            <ErrorCard title={t('errors.loadJobs')} error={jobsQ.error} />
          ) : jobs.length === 0 ? (
            <div className="text-muted text-sm">
              <Trans
                ns="exec"
                i18nKey="empty"
                components={{ code: <code /> }}
              />
            </div>
          ) : (
            <>
              <div className="space-y-1">
                <Label htmlFor="exec-job">{t('fields.jobId')}</Label>
                <Select
                  id="exec-job"
                  value={jobId}
                  onChange={(e) => setJobId(e.target.value)}
                >
                  <option value="">{t('options.pickOne')}</option>
                  {jobs.map((j) => (
                    <option key={j.id} value={j.id}>
                      {j.id} — v{j.version}
                      {j.description ? ` · ${j.description}` : ''}
                    </option>
                  ))}
                </Select>
              </div>

              <div className="space-y-1">
                <Label htmlFor="exec-target">{t('fields.target')}</Label>
                <Select
                  id="exec-target"
                  value={mode}
                  onChange={(e) => setMode(e.target.value as TargetMode)}
                >
                  <option value="all">{t('options.all')}</option>
                  <option value="groups">{t('options.groups')}</option>
                  <option value="pcs">{t('options.pcs')}</option>
                </Select>
              </div>

              {mode === 'all' && (
                <div
                  role="alert"
                  className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 p-2 text-sm text-danger"
                >
                  <AlertTriangle className="size-4 mt-0.5 shrink-0" />
                  <span>{t('allWarningInline')}</span>
                </div>
              )}

              {mode === 'groups' && (
                <div className="space-y-1">
                  <Label htmlFor="exec-groups">{t('fields.groups')}</Label>
                  {/* multi-select: pick groups as removable chips, or
                      paste a comma/whitespace-separated bulk list */}
                  <GroupPicker mode="multi" id="exec-groups" value={groups} onChange={setGroups} />
                </div>
              )}
              {mode === 'pcs' && (
                <div className="space-y-1">
                  <Label htmlFor="exec-pcs">{t('fields.pcs')}</Label>
                  {/* multi-select: pick several existing PCs as removable chips */}
                  <PcPicker mode="multi" id="exec-pcs" value={pcs} onChange={setPcs} />
                </div>
              )}

              <div className="space-y-1">
                <Label htmlFor="exec-jitter">{t('fields.jitter')}</Label>
                <Input
                  id="exec-jitter"
                  value={jitter}
                  onChange={(e) => setJitter(e.target.value)}
                  placeholder=""
                />
              </div>
            </>
          )}

          <Button
            onClick={onFire}
            disabled={!canOperate || !jobId || !targetReady || mut.isPending}
            title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
          >
            {mut.isPending
              ? <Loader2 className="size-4 animate-spin" />
              : <Send className="size-4" />}
            {t('submit', { jobId: jobId || t('submitFallback') })}
          </Button>
          {!canOperate && (
            <p className="text-xs text-muted mt-2">{t('rbac.operatorRequired', { ns: 'common' })}</p>
          )}
        </CardContent>
      </Card>

      {mut.error && <ErrorCard title={t('errors.execFailed')} error={mut.error} />}
      {mut.data && (
        <Card>
          <CardHeader>
            <CardTitle>{t('accepted.title')}</CardTitle>
            <CardDescription>
              {t('accepted.summary', {
                count: mut.data.target_count,
                subjects: mut.data.subjects.length,
              })}
            </CardDescription>
          </CardHeader>
          <CardContent>
            <JsonOutput value={mut.data} />
          </CardContent>
        </Card>
      )}
    </div>
  );
}
