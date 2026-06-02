import { useMutation, useQuery } from '@tanstack/react-query';
import { Loader2, Send } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { PcPicker } from '@/components/PcPicker';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { apiFetch } from '@/lib/api';

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

function splitCsv(s: string): string[] {
  return s
    .split(',')
    .map((x) => x.trim())
    .filter(Boolean);
}

export function Exec() {
  const { t } = useTranslation('exec');
  const [jobId, setJobId] = useState('');
  const [mode, setMode] = useState<TargetMode>('all');
  const [groups, setGroups] = useState('');
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

  const onFire = () => {
    const plan: FanoutPlan = {
      target: {
        all: mode === 'all',
        groups: mode === 'groups' ? splitCsv(groups) : [],
        pcs: mode === 'pcs' ? pcs : [],
      },
    };
    if (jitter.trim()) plan.jitter = jitter.trim();
    mut.mutate({ id: jobId, plan });
  };

  const targetReady =
    mode === 'all'
      || (mode === 'groups' && splitCsv(groups).length > 0)
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

              {mode === 'groups' && (
                <div className="space-y-1">
                  <Label htmlFor="exec-groups">{t('fields.groups')}</Label>
                  <Input
                    id="exec-groups"
                    value={groups}
                    onChange={(e) => setGroups(e.target.value)}
                    placeholder={t('placeholders.groups')}
                  />
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
            disabled={!jobId || !targetReady || mut.isPending}
          >
            {mut.isPending
              ? <Loader2 className="size-4 animate-spin" />
              : <Send className="size-4" />}
            {t('submit', { jobId: jobId || t('submitFallback') })}
          </Button>
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
