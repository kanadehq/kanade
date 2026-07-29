import { useMutation } from '@tanstack/react-query';
import { Loader2, Play } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { PcPicker } from '@/components/PcPicker';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Textarea } from '@/components/ui/textarea';
import { apiFetch } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import type { ExecResult } from '@/lib/types';

type RunAsValue = 'system' | 'user' | 'system_gui';

type RunBody = {
  pc_id: string;
  shell: string;
  script: string;
  timeout_secs: number;
  run_as: RunAsValue;
  job_id?: string;
};

export function Run() {
  const { t } = useTranslation('run');
  const { hasRole } = useAuth();
  const confirm = useConfirm();
  const canOperate = hasRole('operator');
  const [pcId, setPcId] = useState('');
  const [shell, setShell] = useState('powershell');
  const [runAs, setRunAs] = useState<RunAsValue>('system');
  const [timeout, setTimeout] = useState(60);
  const [jobId, setJobId] = useState('');
  const [script, setScript] = useState('');

  const mut = useMutation({
    mutationFn: (body: RunBody) =>
      apiFetch<ExecResult>('/api/run', {
        method: 'POST',
        body: JSON.stringify(body),
      }),
  });

  const onSubmit = async () => {
    if (!pcId.trim() || !script.trim()) return;
    const body: RunBody = {
      pc_id: pcId.trim(),
      shell,
      script,
      timeout_secs: timeout,
      run_as: runAs,
    };
    if (jobId.trim()) body.job_id = jobId.trim();

    // Ad-hoc run executes arbitrary script on the target with no undo —
    // confirm the pc / shell / run_as before sending. run_as=system runs
    // as SYSTEM, so the summary spells out which privilege level it lands on.
    // The high-privilege levels (system / system_gui) get danger styling
    // (red confirm, Cancel auto-focused) so an accidental Enter doesn't
    // fire a SYSTEM script.
    const ok = await confirm({
      title: t('confirm.title', { pcId: body.pc_id }),
      description: t('confirm.summary', { shell, runAs }),
      confirmLabel: t('confirm.confirmLabel'),
      cancelLabel: t('confirm.cancelLabel'),
      danger: runAs.startsWith('system'),
    });
    if (!ok) return;

    mut.mutate(body);
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>{t('title')}</CardTitle>
          <CardDescription>
            <Trans ns="run" i18nKey="description" components={{ code: <code /> }} />
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-1 md:grid-cols-5 gap-3">
            <div>
              <Label>{t('fields.pcId')}</Label>
              <PcPicker value={pcId} onChange={setPcId} placeholder={t('placeholders.pcId')} />
            </div>
            <div>
              <Label>{t('fields.shell')}</Label>
              <Select value={shell} onChange={(e) => setShell(e.target.value)}>
                <option value="powershell">powershell</option>
                <option value="pwsh">pwsh</option>
                <option value="cmd">cmd</option>
                <option value="sh">sh</option>
              </Select>
            </div>
            <div>
              <Label>{t('fields.runAs')}</Label>
              <Select value={runAs} onChange={(e) => setRunAs(e.target.value as RunAsValue)}>
                <option value="system">system</option>
                <option value="user">user</option>
                <option value="system_gui">system_gui</option>
              </Select>
            </div>
            <div>
              <Label>{t('fields.timeout')}</Label>
              <Input
                type="number"
                min={1}
                value={timeout}
                onChange={(e) => setTimeout(parseInt(e.target.value, 10) || 60)}
              />
            </div>
            <div>
              <Label>{t('fields.jobId')}</Label>
              <Input value={jobId} onChange={(e) => setJobId(e.target.value)} placeholder={t('placeholders.jobId')} />
            </div>
          </div>
          <div>
            <Label>{t('fields.script')}</Label>
            <Textarea
              value={script}
              onChange={(e) => setScript(e.target.value)}
              placeholder={t('placeholders.script')}
              className="min-h-32"
            />
          </div>
          <Button
            onClick={onSubmit}
            disabled={!canOperate || !pcId.trim() || !script.trim() || mut.isPending}
            title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
          >
            {mut.isPending ? <Loader2 className="size-4 animate-spin" /> : <Play className="size-4" />}
            {t('submit')}
          </Button>
          {!canOperate && (
            <p className="text-xs text-muted mt-2">{t('rbac.operatorRequired', { ns: 'common' })}</p>
          )}
        </CardContent>
      </Card>

      {mut.error && <ErrorCard title={t('errorTitle')} error={mut.error} />}
      {mut.data && (
        <Card>
          <CardHeader>
            <CardTitle>
              {t('result.exitCode')} <span className={mut.data.exit_code === 0 ? 'text-success' : 'text-danger'}>{mut.data.exit_code}</span>
            </CardTitle>
            <CardDescription>
              {mut.data.pc_id} · {mut.data.started_at} → {mut.data.finished_at}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <div>
              <Label>{t('result.stdout')}</Label>
              <JsonOutput value={mut.data.stdout || t('result.empty')} />
            </div>
            {mut.data.stderr && (
              <div>
                <Label>{t('result.stderr')}</Label>
                <JsonOutput value={mut.data.stderr} className="text-danger" />
              </div>
            )}
          </CardContent>
        </Card>
      )}
    </div>
  );
}
