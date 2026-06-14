import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Save, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { toast } from 'sonner';

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
import { apiFetch, formatError } from '@/lib/api';
import type { ConfigScope, EffectiveConfig, EffectiveConfigResponse } from '@/lib/types';

// The string-valued ConfigScope knobs, paired with the EffectiveConfig
// key whose value seeds the field's placeholder (the built-in default
// for the global scope, the inherited value for a group/pc scope).
// `client_display_name` is in here too, but it renders with a bespoke
// placeholder and maxLength below rather than via `ph()` — its
// EffectiveConfig value is `null` when unset (the client supplies its
// own built-in name), so there's no concrete value to show.
const SCOPE_TEXT_FIELDS = [
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

type ScopeForm = {
  target_version: string;
  target_version_jitter: string;
  heartbeat_interval: string;
  host_perf_interval: string;
  // '' = inherit (key dropped → falls through to the layer below);
  // 'true'/'false' pin the flag on this scope.
  process_perf_enabled: '' | 'true' | 'false';
  process_perf_expires_at: string;
  process_perf_top_n: string;
  // Blank → key dropped → inherit (global falls back to the client's
  // own built-in name).
  client_display_name: string;
};

const EMPTY_SCOPE_FORM: ScopeForm = {
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
// inheriting the layer below rather than freezing the current value.
function scopeToForm(s: ConfigScope): ScopeForm {
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

// Build the ConfigScope to PUT from the form, starting from `base` (the
// freshest server scope) so any field this form doesn't manage survives
// the round-trip. A blank field deletes its key — the "inherit the
// layer below" semantics shared by every scope. Returns an i18n error
// key instead of a scope when a field fails client-side validation.
function formToScope(
  form: ScopeForm,
  base: ConfigScope,
): { ok: true; scope: ConfigScope } | { ok: false; errorKey: string } {
  const next: ConfigScope = { ...base };

  for (const { key } of SCOPE_TEXT_FIELDS) {
    const trimmed = form[key].trim();
    // Every SCOPE_TEXT_FIELDS key maps to a `string?` field on
    // ConfigScope (guaranteed by the `satisfies` constraint), so this
    // widened write is safe; the Record cast just avoids a stray `never`.
    if (trimmed) (next as Record<string, unknown>)[key] = trimmed;
    else delete next[key];
  }

  if (form.process_perf_enabled === '') delete next.process_perf_enabled;
  else next.process_perf_enabled = form.process_perf_enabled === 'true';

  const expires = form.process_perf_expires_at.trim();
  if (expires) {
    // The backend parses this as chrono::DateTime<Utc>; a malformed
    // string comes back as an opaque PUT error. Date.parse is lenient
    // (accepts some non-RFC3339 forms) but catches the obvious typos.
    if (Number.isNaN(Date.parse(expires)))
      return { ok: false, errorKey: 'configForm.alerts.invalidExpiresAt' };
    next.process_perf_expires_at = expires;
  } else {
    delete next.process_perf_expires_at;
  }

  const topN = form.process_perf_top_n.trim();
  if (!topN) {
    delete next.process_perf_top_n;
  } else {
    const n = Number(topN);
    if (!Number.isInteger(n) || n <= 0)
      return { ok: false, errorKey: 'configForm.alerts.invalidTopN' };
    next.process_perf_top_n = n;
  }

  return { ok: true, scope: next };
}

// The shared field grid used by every scope tab. `placeholders` is the
// EffectiveConfig a blank field falls back to (built-in defaults for
// global, inherited values for group/pc); `blankHint` explains that
// fall-through in scope-appropriate words.
function ConfigScopeForm({
  value,
  onChange,
  placeholders,
  placeholdersError,
  blankHint,
}: {
  value: ScopeForm;
  onChange: (next: ScopeForm) => void;
  placeholders: EffectiveConfig | undefined;
  placeholdersError?: unknown;
  blankHint: string;
}) {
  const { t } = useTranslation('config');
  const setField = <K extends keyof ScopeForm>(key: K, v: ScopeForm[K]) =>
    onChange({ ...value, [key]: v });

  // Placeholder = what this field resolves to when left blank. A
  // null/unset value (target_version, expiry, client name) has no
  // concrete value, so show a localised "(unset)" instead of empty.
  const ph = (key: keyof EffectiveConfig): string => {
    const v = placeholders?.[key];
    if (v === null || v === undefined) return t('configForm.fields.unsetPlaceholder');
    return String(v);
  };

  return (
    <>
      {placeholdersError ? (
        <ErrorCard title={t('configForm.placeholdersErrorTitle')} error={placeholdersError} />
      ) : null}
      <p className="text-xs text-muted">{blankHint}</p>

      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        <div className="md:col-span-2">
          <Label htmlFor="cfg-client-name">{t('configForm.fields.clientDisplayName.label')}</Label>
          <Input
            id="cfg-client-name"
            value={value.client_display_name}
            onChange={(e) => setField('client_display_name', e.target.value)}
            placeholder={t('configForm.fields.clientDisplayName.placeholder')}
            maxLength={CLIENT_DISPLAY_NAME_MAX}
          />
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.clientDisplayName.hint')}</p>
        </div>
        <div>
          <Label htmlFor="cfg-target-version">{t('configForm.fields.targetVersion.label')}</Label>
          <Input
            id="cfg-target-version"
            value={value.target_version}
            onChange={(e) => setField('target_version', e.target.value)}
            placeholder={ph('target_version')}
          />
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.targetVersion.hint')}</p>
        </div>
        <div>
          <Label htmlFor="cfg-jitter">{t('configForm.fields.targetVersionJitter.label')}</Label>
          <Input
            id="cfg-jitter"
            value={value.target_version_jitter}
            onChange={(e) => setField('target_version_jitter', e.target.value)}
            placeholder={ph('target_version_jitter')}
          />
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.targetVersionJitter.hint')}</p>
        </div>
        <div>
          <Label htmlFor="cfg-heartbeat">{t('configForm.fields.heartbeatInterval.label')}</Label>
          <Input
            id="cfg-heartbeat"
            value={value.heartbeat_interval}
            onChange={(e) => setField('heartbeat_interval', e.target.value)}
            placeholder={ph('heartbeat_interval')}
          />
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.heartbeatInterval.hint')}</p>
        </div>
        <div>
          <Label htmlFor="cfg-host-perf">{t('configForm.fields.hostPerfInterval.label')}</Label>
          <Input
            id="cfg-host-perf"
            value={value.host_perf_interval}
            onChange={(e) => setField('host_perf_interval', e.target.value)}
            placeholder={ph('host_perf_interval')}
          />
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.hostPerfInterval.hint')}</p>
        </div>
        <div>
          <Label htmlFor="cfg-pp-enabled">{t('configForm.fields.processPerfEnabled.label')}</Label>
          <Select
            id="cfg-pp-enabled"
            value={value.process_perf_enabled}
            onChange={(e) =>
              setField('process_perf_enabled', e.target.value as ScopeForm['process_perf_enabled'])
            }
          >
            <option value="">
              {t('configForm.fields.processPerfEnabled.inherit', {
                value: placeholders
                  ? t(
                      placeholders.process_perf_enabled
                        ? 'configForm.fields.processPerfEnabled.on'
                        : 'configForm.fields.processPerfEnabled.off',
                    )
                  : '…',
              })}
            </option>
            <option value="true">{t('configForm.fields.processPerfEnabled.on')}</option>
            <option value="false">{t('configForm.fields.processPerfEnabled.off')}</option>
          </Select>
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.processPerfEnabled.hint')}</p>
        </div>
        <div>
          <Label htmlFor="cfg-pp-topn">{t('configForm.fields.processPerfTopN.label')}</Label>
          <Input
            id="cfg-pp-topn"
            type="number"
            min={1}
            value={value.process_perf_top_n}
            onChange={(e) => setField('process_perf_top_n', e.target.value)}
            placeholder={ph('process_perf_top_n')}
          />
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.processPerfTopN.hint')}</p>
        </div>
        <div className="md:col-span-2">
          <Label htmlFor="cfg-pp-expires">{t('configForm.fields.processPerfExpiresAt.label')}</Label>
          <Input
            id="cfg-pp-expires"
            value={value.process_perf_expires_at}
            onChange={(e) => setField('process_perf_expires_at', e.target.value)}
            placeholder={t('configForm.fields.processPerfExpiresAt.placeholder')}
          />
          <p className="mt-1 text-xs text-muted">{t('configForm.fields.processPerfExpiresAt.hint')}</p>
        </div>
      </div>
    </>
  );
}

function GlobalTab() {
  const { t } = useTranslation('config');
  const qc = useQueryClient();
  // #520: staleTime Infinity + no focus refetch — the app-wide defaults
  // re-fired this query on every tab focus, and the seeding effect below
  // replaced the operator's unsaved draft. With these options the only
  // refetch is our own invalidation after a successful save.
  const { data, error, isLoading } = useQuery({
    queryKey: ['config', 'global'],
    queryFn: () => apiFetch<ConfigScope>('/api/config'),
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  // Built-in floor values, sourced from the backend so the placeholders
  // never drift from the Rust source of truth (a default change like
  // #491's 10m jitter shows up here for free).
  const { data: defaults, error: defaultsError } = useQuery({
    queryKey: ['config', 'defaults'],
    queryFn: () => apiFetch<EffectiveConfig>('/api/config/defaults'),
    staleTime: Infinity,
  });
  const [form, setForm] = useState<ScopeForm>(EMPTY_SCOPE_FORM);
  // Seed once per load session (#520 guard); reset after a save so the
  // invalidated refetch re-seeds with the server-normalised scope.
  const seeded = useRef(false);
  useEffect(() => {
    if (data && !seeded.current) {
      setForm(scopeToForm(data));
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

  const submit = () => {
    if (!data) {
      toast.error(t('configForm.alerts.notLoaded'));
      return;
    }
    const r = formToScope(form, data);
    if (!r.ok) {
      toast.error(t(r.errorKey));
      return;
    }
    save.mutate(r.scope);
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
        <ConfigScopeForm
          value={form}
          onChange={setForm}
          placeholders={defaults}
          placeholdersError={defaultsError}
          blankHint={t('global.blankHint')}
        />
        {/* Disabled until the global snapshot has loaded, so the
            read-modify-write spread is never built from `{}`. */}
        <Button onClick={submit} disabled={save.isPending || !data}>
          {save.isPending ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
          {t('global.saveButton')}
        </Button>
        {save.error && <ErrorCard title={t('global.saveErrorTitle')} error={save.error} />}
      </CardContent>
    </Card>
  );
}

// Editor for a single group / pc override. `kind` is fixed by the tab,
// so there's no scope-type selector — the operator picks a name, loads
// it, and edits the same form as global. Partial overrides: a blank
// field deletes its key and falls through to the layer below, whose
// resolved value is shown as the placeholder.
function ScopeTab({ kind }: { kind: 'groups' | 'pcs' }) {
  const { t } = useTranslation('config');
  const qc = useQueryClient();
  const confirm = useConfirm();
  const [name, setName] = useState('');
  // Latest name, readable from async mutation callbacks (which close
  // over the render's `name` at mutate time) so a slow load can drop a
  // response for a name the operator has since moved off of.
  const nameRef = useRef(name);
  nameRef.current = name;
  const [form, setForm] = useState<ScopeForm>(EMPTY_SCOPE_FORM);
  // The scope as last loaded from the server — both the read-modify-write
  // base and the "has a scope been loaded?" flag. Null until load.
  const [loaded, setLoaded] = useState<ConfigScope | null>(null);
  const [placeholders, setPlaceholders] = useState<EffectiveConfig | undefined>(undefined);
  const [placeholdersError, setPlaceholdersError] = useState<unknown>(undefined);

  const cfgUrl = `/api/${kind}/${encodeURIComponent(name)}/config`;

  // A pc_id and a group name aren't interchangeable, and an unloaded
  // name must not be saveable — reset the loaded state whenever the
  // selection changes so a stale form can't be PUT under a new name.
  const onName = (v: string) => {
    setName(v);
    setLoaded(null);
    setPlaceholders(undefined);
    setPlaceholdersError(undefined);
    setForm(EMPTY_SCOPE_FORM);
  };

  const load = useMutation({
    mutationFn: async () => {
      // Capture the name this load is for; the picker may change before
      // the response lands.
      const forName = name;
      const base = `/api/${kind}/${encodeURIComponent(forName)}/config`;
      // The scope itself seeds the form and is required. The inherited
      // config only feeds placeholders (read-only UX sugar), so its
      // failure must NOT block opening the editor — fetch it separately
      // and degrade to "(unset)" placeholders + an inline error.
      const scope = await apiFetch<ConfigScope>(base);
      let inherited: EffectiveConfig | undefined;
      let inheritedError: unknown;
      try {
        inherited = await apiFetch<EffectiveConfig>(`${base}/inherited`);
      } catch (e) {
        inheritedError = e;
      }
      return { scope, inherited, inheritedError, forName };
    },
    onSuccess: ({ scope, inherited, inheritedError, forName }) => {
      // Drop a response whose name the operator has since moved off of,
      // so a stale scope can't land under the new selection.
      if (forName !== nameRef.current) return;
      setForm(scopeToForm(scope));
      setLoaded(scope);
      setPlaceholders(inherited);
      setPlaceholdersError(inheritedError);
    },
  });

  const save = useMutation({
    mutationFn: (body: ConfigScope) =>
      apiFetch<ConfigScope>(cfgUrl, { method: 'PUT', body: JSON.stringify(body) }),
    onSuccess: (saved) => {
      // Refresh the read-modify-write base from the server echo so a
      // follow-up edit spreads the just-persisted (server-normalised)
      // scope, mirroring GlobalTab's post-save re-seed.
      setLoaded(saved);
      qc.invalidateQueries({ queryKey: ['config'] });
      toast.success(t('scope.toast.saveSuccess', { url: cfgUrl }));
    },
    onError: (e) => toast.error(t('scope.toast.saveFailure', { error: formatError(e) })),
  });

  const del = useMutation({
    mutationFn: () => apiFetch(cfgUrl, { method: 'DELETE' }),
    onSuccess: () => {
      setForm(EMPTY_SCOPE_FORM);
      setLoaded(null);
      qc.invalidateQueries({ queryKey: ['config'] });
      toast.success(t('scope.toast.deleteSuccess', { url: cfgUrl }));
    },
    onError: (e) => toast.error(t('scope.toast.deleteFailure', { error: formatError(e) })),
  });

  const submit = () => {
    if (!loaded) return;
    const r = formToScope(form, loaded);
    if (!r.ok) {
      toast.error(t(r.errorKey));
      return;
    }
    save.mutate(r.scope);
  };

  const blankHint =
    kind === 'groups'
      ? `${t('scope.blankHint')} ${t('scope.blankHintGroupNote')}`
      : t('scope.blankHint');

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t(kind === 'groups' ? 'scope.groupTitle' : 'scope.pcTitle')}</CardTitle>
        <CardDescription>{t('scope.description')}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid grid-cols-1 md:grid-cols-[1fr_auto] gap-3 items-end">
          <div>
            <Label>{t(kind === 'groups' ? 'scope.labels.group' : 'scope.labels.pc')}</Label>
            {kind === 'pcs' ? (
              <PcPicker value={name} onChange={onName} placeholder={t('scope.placeholders.pc')} />
            ) : (
              <GroupPicker value={name} onChange={onName} placeholder={t('scope.placeholders.group')} />
            )}
          </div>
          <Button
            variant="secondary"
            disabled={!name.trim() || load.isPending}
            onClick={() => load.mutate()}
          >
            {load.isPending ? <Loader2 className="size-4 animate-spin" /> : null}
            {t('scope.buttons.load')}
          </Button>
        </div>

        {load.error && <ErrorCard title={t('scope.errorTitle')} error={load.error} />}

        {loaded && (
          <>
            <ConfigScopeForm
              value={form}
              onChange={setForm}
              placeholders={placeholders}
              placeholdersError={placeholdersError}
              blankHint={blankHint}
            />
            <div className="flex gap-2">
              <Button onClick={submit} disabled={save.isPending}>
                {save.isPending ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <Save className="size-4" />
                )}
                {t('scope.buttons.save')}
              </Button>
              <Button
                variant="danger"
                disabled={del.isPending}
                onClick={async () => {
                  const ok = await confirm({
                    title: t('scope.confirm.deleteTitle', { url: cfgUrl }),
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
            {(save.error || del.error) && (
              <ErrorCard title={t('scope.errorTitle')} error={save.error ?? del.error} />
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

function EffectiveResolver() {
  const { t } = useTranslation('config');
  const [pcId, setPcId] = useState('');
  const mut = useMutation({
    mutationFn: () =>
      apiFetch<EffectiveConfigResponse>(`/api/agents/${encodeURIComponent(pcId)}/effective_config`),
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('effective.title')}</CardTitle>
        <CardDescription>
          <Trans ns="config" i18nKey="effective.description" components={{ code: <code /> }} />
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex gap-3 items-end">
          <div className="flex-1">
            <Label>{t('effective.labels.pcId')}</Label>
            <PcPicker value={pcId} onChange={setPcId} placeholder={t('effective.placeholders.pcId')} />
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
                  {mut.data.warnings.map((w, i) => (
                    <li key={i}>{w}</li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

const TABS = ['global', 'groups', 'pcs'] as const;
type Tab = (typeof TABS)[number];

export function Config() {
  const { t } = useTranslation('config');
  const [tab, setTab] = useState<Tab>('global');

  return (
    <div className="space-y-4">
      <div
        role="tablist"
        aria-label={t('tabs.label')}
        className="inline-flex rounded-md border border-border bg-card text-sm overflow-hidden"
      >
        {TABS.map((k) => (
          <button
            key={k}
            type="button"
            role="tab"
            aria-selected={tab === k}
            // Roving tabindex: only the active tab is in the Tab order,
            // so Tab moves to the panel rather than cycling all three
            // tab buttons (WAI-ARIA Tabs pattern).
            tabIndex={tab === k ? 0 : -1}
            onClick={() => setTab(k)}
            className={tab === k ? 'px-4 h-9 bg-accent/15 text-accent' : 'px-4 h-9 hover:bg-accent/5'}
          >
            {t(`tabs.${k}`)}
          </button>
        ))}
      </div>

      {tab === 'global' && <GlobalTab />}
      {/* Distinct keys so React mounts a fresh ScopeTab per kind — a
          group name/scope can never carry over into the PC tab. */}
      {tab === 'groups' && <ScopeTab key="groups" kind="groups" />}
      {tab === 'pcs' && <ScopeTab key="pcs" kind="pcs" />}

      <EffectiveResolver />
    </div>
  );
}
