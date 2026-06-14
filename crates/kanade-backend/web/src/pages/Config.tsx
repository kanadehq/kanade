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
import type { ConfigScope, EffectiveConfig, EffectiveConfigResponse } from '@/lib/types';

// The string-valued global-scope knobs, paired with the
// EffectiveConfig key whose built-in default seeds the field's
// placeholder. `client_display_name` is in here too (its dedicated
// editor was folded into this form), but it renders with a bespoke
// placeholder and maxLength below rather than via `ph()` — its
// EffectiveConfig default is `null` (the client supplies its own
// built-in name), so there's no concrete floor value to show.
const GLOBAL_TEXT_FIELDS = [
  { key: 'target_version', defaultKey: 'target_version' },
  { key: 'target_version_jitter', defaultKey: 'target_version_jitter' },
  { key: 'heartbeat_interval', defaultKey: 'heartbeat_interval' },
  { key: 'host_perf_interval', defaultKey: 'host_perf_interval' },
  { key: 'client_display_name', defaultKey: 'client_display_name' },
] as const satisfies ReadonlyArray<{
  key: keyof ConfigScope;
  defaultKey: keyof EffectiveConfig;
}>;

// The name becomes the all-users Start-Menu `.lnk` filename; a path
// past Windows MAX_PATH (~260, minus the ~57-char Start-Menu prefix)
// would fail/truncate. 120 is far above any real product name yet
// safely under the limit (Claude review #670).
const CLIENT_DISPLAY_NAME_MAX = 120;

type GlobalForm = {
  target_version: string;
  target_version_jitter: string;
  heartbeat_interval: string;
  host_perf_interval: string;
  // '' = inherit (key dropped → falls through to the built-in floor);
  // 'true'/'false' pin the flag on the whole fleet.
  process_perf_enabled: '' | 'true' | 'false';
  process_perf_expires_at: string;
  process_perf_top_n: string;
  // Blank → key dropped → the client renders its own built-in name.
  client_display_name: string;
};

const EMPTY_GLOBAL_FORM: GlobalForm = {
  target_version: '',
  target_version_jitter: '',
  heartbeat_interval: '',
  host_perf_interval: '',
  process_perf_enabled: '',
  process_perf_expires_at: '',
  process_perf_top_n: '',
  client_display_name: '',
};

// A field left blank means "this scope doesn't set it" — exactly the
// `None`/key-absent state on the Rust side — so the value keeps
// inheriting the (evolving) built-in default rather than freezing it.
function scopeToGlobalForm(s: ConfigScope): GlobalForm {
  return {
    target_version: s.target_version ?? '',
    target_version_jitter: s.target_version_jitter ?? '',
    heartbeat_interval: s.heartbeat_interval ?? '',
    host_perf_interval: s.host_perf_interval ?? '',
    // `== null` (not `=== undefined`): the backend omits unset keys via
    // skip_serializing_if so they arrive `undefined` today, but a JSON
    // `null` must map to the same "inherit" state — otherwise it would
    // read as an explicit 'false' / the string "null". Matches the
    // nullish `?? ''` the string fields above already use.
    process_perf_enabled:
      s.process_perf_enabled == null ? '' : s.process_perf_enabled ? 'true' : 'false',
    process_perf_expires_at: s.process_perf_expires_at ?? '',
    process_perf_top_n: s.process_perf_top_n == null ? '' : String(s.process_perf_top_n),
    client_display_name: s.client_display_name ?? '',
  };
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
  // Built-in floor values, sourced from the backend so the
  // placeholders never drift from the Rust source of truth (a default
  // change like #491's 10m jitter shows up here for free). Read-only,
  // so the app-wide caching defaults are fine — no #520 concern.
  const { data: defaults, error: defaultsError } = useQuery({
    queryKey: ['config', 'defaults'],
    queryFn: () => apiFetch<EffectiveConfig>('/api/config/defaults'),
    staleTime: Infinity,
  });
  const [form, setForm] = useState<GlobalForm>(EMPTY_GLOBAL_FORM);
  // Seed the form once per load session (same #520 guard as before);
  // reset after a save so the invalidated refetch re-seeds with the
  // server-normalised scope.
  const seeded = useRef(false);
  useEffect(() => {
    if (data && !seeded.current) {
      setForm(scopeToGlobalForm(data));
      seeded.current = true;
    }
  }, [data]);

  const setField = <K extends keyof GlobalForm>(key: K, value: GlobalForm[K]) =>
    setForm((f) => ({ ...f, [key]: value }));

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

  // Default value rendered as a field's placeholder. null/None
  // (target_version, expires_at) has no concrete floor, so we show a
  // localised "(unset)" hint instead of an empty box.
  const ph = (key: keyof EffectiveConfig): string => {
    const v = defaults?.[key];
    if (v === null || v === undefined) return t('global.fields.unsetPlaceholder');
    return String(v);
  };

  const submit = () => {
    // Guard: this is a full-replace PUT on the whole global scope, so
    // building it before the GET populated `data` would spread `{}` and
    // drop every field. The button is also disabled in this state.
    if (!data) {
      toast.error(t('global.alerts.notLoaded'));
      return;
    }
    // Read-modify-write: start from the freshest server scope so any
    // field this form doesn't manage (e.g. a knob a future backend
    // adds) survives the round-trip rather than being dropped.
    const next: ConfigScope = { ...data };

    for (const { key } of GLOBAL_TEXT_FIELDS) {
      const trimmed = form[key].trim();
      // Every GLOBAL_TEXT_FIELDS key maps to a `string?` field on
      // ConfigScope (guaranteed by the `satisfies` constraint on the
      // array), so this widened write is safe — the Record cast just
      // spares a future reader from wondering why `never` was needed.
      if (trimmed) (next as Record<string, unknown>)[key] = trimmed;
      else delete next[key];
    }

    if (form.process_perf_enabled === '') delete next.process_perf_enabled;
    else next.process_perf_enabled = form.process_perf_enabled === 'true';

    const expires = form.process_perf_expires_at.trim();
    if (expires) {
      // The backend parses this as chrono::DateTime<Utc>; a malformed
      // string comes back as an opaque PUT error. Catch the obvious
      // typos here. Date.parse is lenient (accepts some non-RFC3339
      // forms) but good enough to surface the format requirement early.
      if (Number.isNaN(Date.parse(expires))) {
        toast.error(t('global.alerts.invalidExpiresAt'));
        return;
      }
      next.process_perf_expires_at = expires;
    } else {
      delete next.process_perf_expires_at;
    }

    const topN = form.process_perf_top_n.trim();
    if (!topN) {
      delete next.process_perf_top_n;
    } else {
      const n = Number(topN);
      if (!Number.isInteger(n) || n <= 0) {
        toast.error(t('global.alerts.invalidTopN'));
        return;
      }
      next.process_perf_top_n = n;
    }

    save.mutate(next);
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('global.title')}</CardTitle>
        <CardDescription>{t('global.description')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {isLoading && (
          <div className="text-muted flex items-center gap-2">
            <Loader2 className="size-4 animate-spin" />
            {t('global.loading')}
          </div>
        )}
        {error && <ErrorCard title={t('global.loadErrorTitle')} error={error} />}
        {defaultsError && <ErrorCard title={t('global.loadErrorTitle')} error={defaultsError} />}
        <p className="text-xs text-muted">{t('global.blankHint')}</p>

        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          <div className="md:col-span-2">
            <Label htmlFor="cfg-client-name">{t('global.fields.clientDisplayName.label')}</Label>
            <Input
              id="cfg-client-name"
              value={form.client_display_name}
              onChange={(e) => setField('client_display_name', e.target.value)}
              placeholder={t('global.fields.clientDisplayName.placeholder')}
              maxLength={CLIENT_DISPLAY_NAME_MAX}
            />
            <p className="mt-1 text-xs text-muted">{t('global.fields.clientDisplayName.hint')}</p>
          </div>
          <div>
            <Label htmlFor="cfg-target-version">{t('global.fields.targetVersion.label')}</Label>
            <Input
              id="cfg-target-version"
              value={form.target_version}
              onChange={(e) => setField('target_version', e.target.value)}
              placeholder={ph('target_version')}
            />
            <p className="mt-1 text-xs text-muted">{t('global.fields.targetVersion.hint')}</p>
          </div>
          <div>
            <Label htmlFor="cfg-jitter">{t('global.fields.targetVersionJitter.label')}</Label>
            <Input
              id="cfg-jitter"
              value={form.target_version_jitter}
              onChange={(e) => setField('target_version_jitter', e.target.value)}
              placeholder={ph('target_version_jitter')}
            />
            <p className="mt-1 text-xs text-muted">
              {t('global.fields.targetVersionJitter.hint')}
            </p>
          </div>
          <div>
            <Label htmlFor="cfg-heartbeat">{t('global.fields.heartbeatInterval.label')}</Label>
            <Input
              id="cfg-heartbeat"
              value={form.heartbeat_interval}
              onChange={(e) => setField('heartbeat_interval', e.target.value)}
              placeholder={ph('heartbeat_interval')}
            />
            <p className="mt-1 text-xs text-muted">
              {t('global.fields.heartbeatInterval.hint')}
            </p>
          </div>
          <div>
            <Label htmlFor="cfg-host-perf">{t('global.fields.hostPerfInterval.label')}</Label>
            <Input
              id="cfg-host-perf"
              value={form.host_perf_interval}
              onChange={(e) => setField('host_perf_interval', e.target.value)}
              placeholder={ph('host_perf_interval')}
            />
            <p className="mt-1 text-xs text-muted">{t('global.fields.hostPerfInterval.hint')}</p>
          </div>
          <div>
            <Label htmlFor="cfg-pp-enabled">{t('global.fields.processPerfEnabled.label')}</Label>
            <Select
              id="cfg-pp-enabled"
              value={form.process_perf_enabled}
              onChange={(e) =>
                setField('process_perf_enabled', e.target.value as GlobalForm['process_perf_enabled'])
              }
            >
              <option value="">
                {t('global.fields.processPerfEnabled.inherit', {
                  value: defaults
                    ? t(
                        defaults.process_perf_enabled
                          ? 'global.fields.processPerfEnabled.on'
                          : 'global.fields.processPerfEnabled.off',
                      )
                    : '…',
                })}
              </option>
              <option value="true">{t('global.fields.processPerfEnabled.on')}</option>
              <option value="false">{t('global.fields.processPerfEnabled.off')}</option>
            </Select>
            <p className="mt-1 text-xs text-muted">
              {t('global.fields.processPerfEnabled.hint')}
            </p>
          </div>
          <div>
            <Label htmlFor="cfg-pp-topn">{t('global.fields.processPerfTopN.label')}</Label>
            <Input
              id="cfg-pp-topn"
              type="number"
              min={1}
              value={form.process_perf_top_n}
              onChange={(e) => setField('process_perf_top_n', e.target.value)}
              placeholder={ph('process_perf_top_n')}
            />
            <p className="mt-1 text-xs text-muted">{t('global.fields.processPerfTopN.hint')}</p>
          </div>
          <div className="md:col-span-2">
            <Label htmlFor="cfg-pp-expires">{t('global.fields.processPerfExpiresAt.label')}</Label>
            <Input
              id="cfg-pp-expires"
              value={form.process_perf_expires_at}
              onChange={(e) => setField('process_perf_expires_at', e.target.value)}
              placeholder={t('global.fields.processPerfExpiresAt.placeholder')}
            />
            <p className="mt-1 text-xs text-muted">
              {t('global.fields.processPerfExpiresAt.hint')}
            </p>
          </div>
        </div>

        {/* Disabled until the global snapshot has loaded, so the
            read-modify-write spread is never built from `{}` (which
            would drop every field on save). */}
        <Button onClick={submit} disabled={save.isPending || !data}>
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
