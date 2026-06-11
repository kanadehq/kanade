import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Save, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Textarea } from '@/components/ui/textarea';
import { apiFetch, formatError } from '@/lib/api';
import type { ConfigScope, EffectiveConfigResponse } from '@/lib/types';

function GlobalEditor() {
  const { t } = useTranslation('config');
  const qc = useQueryClient();
  // #520: staleTime Infinity + no focus refetch — the app-wide
  // defaults (staleTime 0, refetchOnWindowFocus) re-fired this query
  // on every tab focus, and the seeding effect below replaced the
  // operator's unsaved draft with whatever the server returned.
  // With these options the only refetch is our own invalidation
  // after a successful save.
  const { data, error, isLoading } = useQuery({
    queryKey: ['config', 'global'],
    queryFn: () => apiFetch<ConfigScope>('/api/config'),
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  const [draft, setDraft] = useState<string>('');
  // Seed the textarea once per load session (same guard as
  // YamlEditorDialog); reset after a save so the invalidated
  // refetch re-seeds with the server-normalised result.
  const seeded = useRef(false);
  useEffect(() => {
    if (data && !seeded.current) {
      setDraft(JSON.stringify(data, null, 2));
      seeded.current = true;
    }
  }, [data]);

  const save = useMutation({
    mutationFn: (body: ConfigScope) =>
      apiFetch<ConfigScope>('/api/config', { method: 'PUT', body: JSON.stringify(body) }),
    onSuccess: () => {
      seeded.current = false;
      qc.invalidateQueries({ queryKey: ['config', 'global'] });
      toast.success(t('global.toast.saveSuccess'));
    },
    onError: (e) => toast.error(t('global.toast.saveFailure', { error: formatError(e) })),
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('global.title')}</CardTitle>
        <CardDescription>{t('global.description')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {isLoading && <div className="text-muted flex items-center gap-2"><Loader2 className="size-4 animate-spin" />{t('global.loading')}</div>}
        {error && <ErrorCard title={t('global.loadErrorTitle')} error={error} />}
        <Textarea value={draft} onChange={(e) => setDraft(e.target.value)} className="min-h-40" />
        <Button
          onClick={() => {
            try {
              save.mutate(JSON.parse(draft));
            } catch (e) {
              toast.error(t('global.alerts.invalidJson', { error: (e as Error).message }));
            }
          }}
          disabled={save.isPending}
        >
          {save.isPending ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
          {t('global.saveButton')}
        </Button>
        {save.error && <ErrorCard title={t('global.saveErrorTitle')} error={save.error} />}
      </CardContent>
    </Card>
  );
}

function ScopeEditor() {
  const { t } = useTranslation('config');
  const qc = useQueryClient();
  const confirm = useConfirm();
  const [kind, setKind] = useState<'groups' | 'pcs'>('groups');
  const [name, setName] = useState('');
  const [body, setBody] = useState('');
  const url = () => (kind === 'groups' ? `/api/groups/${encodeURIComponent(name)}/config` : `/api/pcs/${encodeURIComponent(name)}/config`);

  const load = useMutation({
    mutationFn: () => apiFetch<ConfigScope>(url()),
    onSuccess: (r) => setBody(JSON.stringify(r, null, 2)),
  });
  const save = useMutation({
    mutationFn: (b: ConfigScope) => apiFetch(url(), { method: 'PUT', body: JSON.stringify(b) }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['config'] });
      toast.success(t('scope.toast.saveSuccess', { url: url() }));
    },
    onError: (e) => toast.error(t('scope.toast.saveFailure', { error: formatError(e) })),
  });
  const del = useMutation({
    mutationFn: () => apiFetch(url(), { method: 'DELETE' }),
    onSuccess: () => {
      setBody('');
      qc.invalidateQueries({ queryKey: ['config'] });
      toast.success(t('scope.toast.deleteSuccess', { url: url() }));
    },
    onError: (e) => toast.error(t('scope.toast.deleteFailure', { error: formatError(e) })),
  });

  const lastError = load.error ?? save.error ?? del.error;

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('scope.title')}</CardTitle>
        <CardDescription>{t('scope.description')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="grid grid-cols-1 md:grid-cols-[160px_1fr_auto] gap-3 items-end">
          <div>
            <Label>{t('scope.labels.scope')}</Label>
            <Select value={kind} onChange={(e) => setKind(e.target.value as 'groups' | 'pcs')}>
              <option value="groups">{t('scope.kindOptions.groups')}</option>
              <option value="pcs">{t('scope.kindOptions.pcs')}</option>
            </Select>
          </div>
          <div>
            <Label>{t('scope.labels.name')}</Label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder={t('scope.placeholders.name')} />
          </div>
          <Button variant="secondary" disabled={!name.trim() || load.isPending} onClick={() => load.mutate()}>
            {t('scope.buttons.load')}
          </Button>
        </div>
        <Textarea
          value={body}
          onChange={(e) => setBody(e.target.value)}
          className="min-h-32"
          placeholder={t('scope.placeholders.body')}
        />
        <div className="flex gap-2">
          <Button
            onClick={() => {
              try {
                save.mutate(JSON.parse(body));
              } catch (e) {
                toast.error(t('scope.alerts.invalidJson', { error: (e as Error).message }));
              }
            }}
            disabled={!name.trim() || save.isPending}
          >
            {save.isPending ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
            {t('scope.buttons.save')}
          </Button>
          <Button
            variant="danger"
            disabled={!name.trim() || del.isPending}
            onClick={async () => {
              const ok = await confirm({
                title: t('scope.confirm.deleteTitle', { url: url() }),
                description: t('scope.confirm.deleteDescription'),
                confirmLabel: t('scope.confirm.deleteLabel'),
                danger: true,
              });
              if (ok) del.mutate();
            }}
          >
            <Trash2 className="size-3.5" />
            {t('scope.buttons.delete')}
          </Button>
        </div>
        {lastError && <ErrorCard title={t('scope.errorTitle')} error={lastError} />}
      </CardContent>
    </Card>
  );
}

function EffectiveResolver() {
  const { t } = useTranslation('config');
  const [pcId, setPcId] = useState('');
  const mut = useMutation({
    mutationFn: () => apiFetch<EffectiveConfigResponse>(`/api/agents/${encodeURIComponent(pcId)}/effective_config`),
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('effective.title')}</CardTitle>
        <CardDescription>
          <Trans
            ns="config"
            i18nKey="effective.description"
            components={{ code: <code /> }}
          />
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex gap-3 items-end">
          <div className="flex-1">
            <Label>{t('effective.labels.pcId')}</Label>
            <Input value={pcId} onChange={(e) => setPcId(e.target.value)} placeholder={t('effective.placeholders.pcId')} />
          </div>
          <Button onClick={() => mut.mutate()} disabled={!pcId.trim() || mut.isPending} variant="secondary">
            {t('effective.buttons.resolve')}
          </Button>
        </div>
        {mut.error && <ErrorCard title={t('effective.errorTitle')} error={mut.error} />}
        {mut.data && (
          <div className="space-y-2">
            <JsonOutput value={mut.data.effective} />
            {mut.data.warnings.length > 0 && (
              <div className="text-xs">
                <Label>{t('effective.labels.warnings')}</Label>
                <ul className="list-disc pl-5 text-amber">
                  {mut.data.warnings.map((w, i) => <li key={i}>{w}</li>)}
                </ul>
              </div>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function Config() {
  return (
    <div className="space-y-4">
      <GlobalEditor />
      <ScopeEditor />
      <EffectiveResolver />
    </div>
  );
}
