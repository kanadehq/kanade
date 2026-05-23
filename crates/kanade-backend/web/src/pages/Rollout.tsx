import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { CheckCircle2, Loader2, Rocket, Trash2, Upload } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { AgentRow } from '@/lib/types';
import { fmtIsoLocal } from '@/lib/utils';

type ReleaseRow = {
  version: string;
  size: number;
  digest: string | null;
  modified: string | null;
};

type ScopeKind = 'global' | 'group' | 'pc';

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

export function Rollout() {
  const { t } = useTranslation('rollout');
  const qc = useQueryClient();
  const [version, setVersion] = useState('');
  const [scopeKind, setScopeKind] = useState<ScopeKind>('group');
  const [scopeValue, setScopeValue] = useState('canary');
  const [jitter, setJitter] = useState('5m');
  const [uploadFile, setUploadFile] = useState<File | null>(null);

  const releasesQ = useQuery({
    queryKey: ['agent-releases'],
    queryFn: () => apiFetch<ReleaseRow[]>('/api/agents/releases'),
  });
  const agentsQ = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
  });

  const upload = useMutation({
    mutationFn: async () => {
      if (!uploadFile) throw new Error(t('upload.noFileError'));
      const fd = new FormData();
      fd.append('file', uploadFile);
      const token = localStorage.getItem('kanade_token') ?? '';
      const headers: Record<string, string> = { 'X-Kanade-Source': 'spa' };
      if (token) headers.Authorization = `Bearer ${token}`;
      const res = await fetch('/api/agents/publish', {
        method: 'POST',
        body: fd,
        headers,
      });
      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText} — ${await res.text()}`);
      }
      return (await res.json()) as { version: string; size: number; digest: string | null };
    },
    onSuccess: (data) => {
      qc.invalidateQueries({ queryKey: ['agent-releases'] });
      // Pre-select the freshly uploaded version for the rollout
      // step right below.
      setVersion(data.version);
      setUploadFile(null);
    },
  });

  const rollout = useMutation({
    mutationFn: async (overrideVersion?: string) => {
      const ver = overrideVersion ?? version;
      type Scope = { type: 'global' } | { type: 'group'; value: string } | { type: 'pc'; value: string };
      const scope: Scope =
        scopeKind === 'global'
          ? { type: 'global' }
          : scopeKind === 'group'
          ? { type: 'group', value: scopeValue }
          : { type: 'pc', value: scopeValue };
      return apiFetch<{ version: string; scope_label: string; scope_key: string; jitter: string | null }>(
        '/api/agents/rollout',
        {
          method: 'POST',
          body: JSON.stringify({
            version: ver,
            scope,
            jitter: jitter || undefined,
          }),
        },
      );
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['effective'] });
    },
  });

  const remove = useMutation({
    mutationFn: async (v: string) => {
      const token = localStorage.getItem('kanade_token') ?? '';
      const headers: Record<string, string> = { 'X-Kanade-Source': 'spa' };
      if (token) headers.Authorization = `Bearer ${token}`;
      const res = await fetch(`/api/agents/releases/${encodeURIComponent(v)}`, {
        method: 'DELETE',
        headers,
      });
      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText} — ${await res.text()}`);
      }
      return v;
    },
    onSuccess: (v) => {
      qc.invalidateQueries({ queryKey: ['agent-releases'] });
      if (version === v) setVersion('');
    },
  });

  const canSubmit =
    !!version &&
    (scopeKind === 'global' || (!!scopeValue && scopeValue.length > 0)) &&
    !rollout.isPending;

  const scopeValueLabel =
    scopeKind === 'global'
      ? t('rolloutPanel.scopeValueLabel.none')
      : scopeKind === 'group'
      ? t('rolloutPanel.scopeValueLabel.group')
      : t('rolloutPanel.scopeValueLabel.pc');

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <span className="text-xs text-muted">
          <Trans ns="rollout" i18nKey="intro" components={{ code: <code /> }} />
        </span>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Upload className="size-5 text-violet" />
            {t('upload.title')}
          </CardTitle>
          <CardDescription>
            <Trans ns="rollout" i18nKey="upload.description" components={{ code: <code /> }} />
          </CardDescription>
        </CardHeader>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          <div className="space-y-1">
            <Label htmlFor="up-file">{t('upload.fileLabel')}</Label>
            <Input
              id="up-file"
              type="file"
              accept=".exe,application/octet-stream,application/x-msdownload"
              onChange={(e) => setUploadFile(e.target.files?.[0] ?? null)}
            />
          </div>
          <div className="space-y-1 flex flex-col justify-end">
            <Button
              onClick={() => upload.mutate()}
              disabled={!uploadFile || upload.isPending}
              className="w-full"
            >
              {upload.isPending ? (
                <Loader2 className="size-4 mr-2 animate-spin" />
              ) : (
                <Upload className="size-4 mr-2" />
              )}
              {t('upload.uploadButton')}
            </Button>
          </div>
        </CardContent>
        <CardContent className="flex items-center gap-3 pt-0">
          {uploadFile && (
            <span className="text-xs text-muted">
              {uploadFile.name} · {fmtSize(uploadFile.size)}
            </span>
          )}
          {upload.isSuccess && upload.data && (
            <span className="flex items-center gap-2 text-sm text-success">
              <CheckCircle2 className="size-4" />
              {t('upload.successPrefix')} <code className="text-xs">{upload.data.version}</code>
              {' '}({fmtSize(upload.data.size)})
            </span>
          )}
        </CardContent>
        {upload.error && (
          <CardContent className="pt-0">
            <ErrorCard title={t('upload.errorTitle')} error={upload.error} />
          </CardContent>
        )}
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Rocket className="size-5 text-violet" />
            {t('rolloutPanel.title')}
          </CardTitle>
          <CardDescription>
            <Trans ns="rollout" i18nKey="rolloutPanel.description" components={{ strong: <strong /> }} />
          </CardDescription>
        </CardHeader>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          <div className="space-y-1">
            <Label htmlFor="ro-version">{t('rolloutPanel.versionLabel')}</Label>
            <Select id="ro-version" value={version} onChange={(e) => setVersion(e.target.value)}>
              <option value="">{t('rolloutPanel.versionPickerPlaceholder')}</option>
              {(releasesQ.data ?? []).map((r) => (
                <option key={r.version} value={r.version}>
                  {r.version} · {fmtSize(r.size)} · {fmtIsoLocal(r.modified)}
                </option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ro-jitter">{t('rolloutPanel.jitterLabel')}</Label>
            <Input
              id="ro-jitter"
              placeholder={t('rolloutPanel.jitterPlaceholder')}
              value={jitter}
              onChange={(e) => setJitter(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="ro-scope-kind">{t('rolloutPanel.scopeLabel')}</Label>
            <Select
              id="ro-scope-kind"
              value={scopeKind}
              onChange={(e) => setScopeKind(e.target.value as ScopeKind)}
            >
              <option value="global">{t('rolloutPanel.scopeOptions.global')}</option>
              <option value="group">{t('rolloutPanel.scopeOptions.group')}</option>
              <option value="pc">{t('rolloutPanel.scopeOptions.pc')}</option>
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ro-scope-value">{scopeValueLabel}</Label>
            {scopeKind === 'pc' ? (
              <Select
                id="ro-scope-value"
                value={scopeValue}
                onChange={(e) => setScopeValue(e.target.value)}
              >
                <option value="">{t('rolloutPanel.versionPickerPlaceholder')}</option>
                {(agentsQ.data ?? []).map((a) => (
                  <option key={a.pc_id} value={a.pc_id}>{a.pc_id}</option>
                ))}
              </Select>
            ) : (
              <Input
                id="ro-scope-value"
                placeholder={
                  scopeKind === 'global'
                    ? t('rolloutPanel.scopeValuePlaceholder.global')
                    : t('rolloutPanel.scopeValuePlaceholder.group')
                }
                value={scopeValue}
                onChange={(e) => setScopeValue(e.target.value)}
                disabled={scopeKind === 'global'}
              />
            )}
          </div>
        </CardContent>
        <CardContent className="flex items-center gap-3 pt-0">
          <Button onClick={() => rollout.mutate(undefined)} disabled={!canSubmit}>
            {rollout.isPending ? (
              <Loader2 className="size-4 mr-2 animate-spin" />
            ) : (
              <Rocket className="size-4 mr-2" />
            )}
            {t('rolloutPanel.rolloutButton')}
          </Button>
          {rollout.isSuccess && rollout.data && (
            <span className="flex items-center gap-2 text-sm text-success">
              <CheckCircle2 className="size-4" />
              {rollout.data.scope_label} → {rollout.data.version}
              {rollout.data.jitter && (
                <Badge variant="violet">
                  {t('rolloutPanel.jitterBadge', { jitter: rollout.data.jitter })}
                </Badge>
              )}
            </span>
          )}
        </CardContent>
        {rollout.error && (
          <CardContent className="pt-0">
            <ErrorCard title={t('rolloutPanel.errorTitle')} error={rollout.error} />
          </CardContent>
        )}
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('releases.title')}</CardTitle>
          <CardDescription>
            <Trans
              ns="rollout"
              i18nKey="releases.description"
              components={{ code: <code />, strong: <strong /> }}
            />
          </CardDescription>
        </CardHeader>
        <CardContent>
          {releasesQ.isLoading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="size-4 animate-spin" />{t('releases.loading')}
            </div>
          ) : releasesQ.error ? (
            <ErrorCard title={t('releases.errorTitle')} error={releasesQ.error} />
          ) : (releasesQ.data ?? []).length === 0 ? (
            <div className="text-muted text-sm">
              <Trans ns="rollout" i18nKey="releases.empty" components={{ code: <code /> }} />
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('releases.columns.version')}</TableHead>
                  <TableHead>{t('releases.columns.size')}</TableHead>
                  <TableHead>{t('releases.columns.modified')}</TableHead>
                  <TableHead>{t('releases.columns.digest')}</TableHead>
                  <TableHead className="text-right">{t('releases.columns.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(releasesQ.data ?? []).map((r) => (
                  <TableRow key={r.version}>
                    <TableCell><code className="text-xs">{r.version}</code></TableCell>
                    <TableCell className="text-muted text-xs">{fmtSize(r.size)}</TableCell>
                    <TableCell className="text-muted text-xs">{fmtIsoLocal(r.modified)}</TableCell>
                    <TableCell className="text-muted text-xs">
                      <code>{r.digest?.slice(0, 24) ?? '—'}</code>
                    </TableCell>
                    <TableCell className="text-right">
                      <div className="flex justify-end gap-2">
                        <Button
                          size="sm"
                          variant="secondary"
                          onClick={() => setVersion(r.version)}
                        >
                          <Rocket className="size-3.5" />
                          {t('releases.actions.rollout')}
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          onClick={() => {
                            if (confirm(t('releases.confirmDelete', { version: r.version }))) {
                              remove.mutate(r.version);
                            }
                          }}
                          disabled={remove.isPending}
                        >
                          <Trash2 className="size-3.5" />
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
          {remove.error && (
            <div className="mt-3">
              <ErrorCard title={t('releases.deleteErrorTitle')} error={remove.error} />
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
