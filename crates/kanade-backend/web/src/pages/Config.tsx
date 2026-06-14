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

// Friendly, single-purpose editor for the operator-facing client
// product name (`client_display_name`). It lives on the global scope
// — the common "one customer = whole fleet" case — but does a
// read-modify-write that spreads the existing global ConfigScope, so
// saving the name never clobbers the other global fields the raw JSON
// editor below manages. Per-group / per-pc overrides still go through
// the ScopeEditor's JSON. Shares the ['config','global'] query with
// GlobalEditor so a save here invalidates both.
function ClientDisplayNameEditor() {
  const { t } = useTranslation('config');
  const qc = useQueryClient();
  const { data, error, isLoading } = useQuery({
    queryKey: ['config', 'global'],
    queryFn: () => apiFetch<ConfigScope>('/api/config'),
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  const [name, setName] = useState('');
  // Track the last *server* value we seeded from (not just a "seeded
  // once" boolean): this editor shares the ['config','global'] query
  // with GlobalEditor, so when that editor saves a new name the value
  // changes underneath us and the input must re-sync. Re-seeding only
  // when the server value actually changes (staleTime Infinity + no
  // focus refetch mean the only refetch is a post-save invalidation)
  // avoids the #520 clobber: an idle refetch returning the same value
  // won't wipe the operator's in-progress edit.
  const lastServerValue = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (!data) return;
    const serverValue = data.client_display_name ?? '';
    if (lastServerValue.current !== serverValue) {
      setName(serverValue);
      lastServerValue.current = serverValue;
    }
  }, [data]);

  const save = useMutation({
    mutationFn: (next: ConfigScope) =>
      apiFetch<ConfigScope>('/api/config', { method: 'PUT', body: JSON.stringify(next) }),
    onSuccess: (_r, vars) => {
      qc.invalidateQueries({ queryKey: ['config', 'global'] });
      toast.success(
        vars.client_display_name
          ? t('clientName.toast.saveSuccess')
          : t('clientName.toast.clearSuccess'),
      );
    },
    onError: (e) => toast.error(t('clientName.toast.saveFailure', { error: formatError(e) })),
  });

  const commit = (value: string) => {
    // Guard: this is a full-replace PUT on the whole global scope, so
    // committing before the GET has populated `data` would spread `{}`
    // and silently drop every other global field (heartbeat_interval,
    // target_version, …). Bail until the snapshot is loaded (CodeRabbit
    // PR #670). The buttons are also disabled in this state, but the
    // guard makes the data-loss path unreachable regardless.
    if (!data) {
      toast.error(t('clientName.toast.notLoaded'));
      return;
    }
    const trimmed = value.trim();
    // Spread the current global scope so other fields survive; an empty
    // value drops the key entirely (→ client falls back to its default).
    const next: ConfigScope = { ...data };
    if (trimmed) next.client_display_name = trimmed;
    else delete next.client_display_name;
    save.mutate(next);
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('clientName.title')}</CardTitle>
        <CardDescription>{t('clientName.description')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {isLoading && (
          <div className="text-muted flex items-center gap-2">
            <Loader2 className="size-4 animate-spin" />
            {t('clientName.loading')}
          </div>
        )}
        {error && <ErrorCard title={t('clientName.loadErrorTitle')} error={error} />}
        <div>
          <Label htmlFor="client-display-name">{t('clientName.label')}</Label>
          <Input
            id="client-display-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={t('clientName.placeholder')}
            // The name becomes the Start-Menu `.lnk` filename; a path
            // past Windows MAX_PATH (~260, minus the ~57-char Start-Menu
            // prefix) would fail/truncate. 120 is far above any real
            // product name yet safely under the limit (Claude review #670).
            maxLength={120}
          />
          <p className="mt-1 text-xs text-muted">{t('clientName.hint')}</p>
        </div>
        <div className="flex gap-2">
          {/* Disabled until the global snapshot is loaded — a save built
              from a missing `data` would full-replace the scope and drop
              other fields (CodeRabbit PR #670). */}
          <Button onClick={() => commit(name)} disabled={save.isPending || !data}>
            {save.isPending ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
            {t('clientName.saveButton')}
          </Button>
          <Button
            variant="secondary"
            disabled={save.isPending || !data || !name.trim()}
            onClick={() => {
              setName('');
              commit('');
            }}
          >
            {t('clientName.clearButton')}
          </Button>
        </div>
        {save.error && <ErrorCard title={t('clientName.saveErrorTitle')} error={save.error} />}
      </CardContent>
    </Card>
  );
}

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
        {/* This raw editor PUTs the whole global scope from its seeded
            snapshot. It seeds once on mount (#520 guard) and doesn't
            re-seed when the Client-display-name editor above saves, so a
            submit here from a pre-name snapshot would silently drop the
            just-saved client_display_name. Warn instead of breaking the
            #520 guard (Claude review PR #670). */}
        <p className="text-xs text-amber">{t('global.overwriteWarning')}</p>
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
      <ClientDisplayNameEditor />
      <GlobalEditor />
      <ScopeEditor />
      <EffectiveResolver />
    </div>
  );
}
