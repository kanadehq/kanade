import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { LANGUAGES, type LanguageCode } from '@/i18n';
import { apiFetch, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import { useTheme, type Theme } from '@/lib/theme';

const TABS = ['personal', 'server'] as const;
type Tab = (typeof TABS)[number];

// Mirrors `MAX_AGENT_PRUNE_DAYS` (kanade_shared::wire::server_settings):
// 100 years. Keeps the cleanup task's date math in range; the backend PUT
// enforces the same bound, this just gives immediate client-side feedback.
const MAX_AGENT_PRUNE_DAYS = 36_500;

/// Backend-side server settings document (`server_settings` KV). Mirrors
/// `kanade_shared::wire::ServerSettings`: every field is nullable, where
/// `null` (or absent) means "unset — fall back to the built-in default".
/// The same shape is returned by `/api/server-settings/defaults` to carry
/// those built-in defaults (rendered as faint placeholders).
interface ServerSettings {
  agent_prune_days: number | null;
  controller_group: string | null;
}

/// Settings page. Two distinct kinds of settings, split into tabs so it's
/// unmistakable which is which:
///   * "personal"  — language / theme, stored in THIS browser only.
///   * "server"    — backend config in NATS KV, shared by everyone and
///                    edited here (operator-gated).
export function Settings() {
  const { t } = useTranslation('settings');
  const [tab, setTab] = useState<Tab>('personal');
  const tabRefs = useRef<Record<Tab, HTMLButtonElement | null>>({ personal: null, server: null });

  // WAI-ARIA Tabs keyboard model: Left/Right (wrapping) + Home/End move
  // focus AND activate (automatic-activation pattern). Without this the
  // roving tabIndex below leaves keyboard users unable to switch panels.
  const onKeyDown = (e: ReactKeyboardEvent) => {
    const i = TABS.indexOf(tab);
    let next: Tab | undefined;
    if (e.key === 'ArrowRight') next = TABS[(i + 1) % TABS.length];
    else if (e.key === 'ArrowLeft') next = TABS[(i - 1 + TABS.length) % TABS.length];
    else if (e.key === 'Home') next = TABS[0];
    else if (e.key === 'End') next = TABS[TABS.length - 1];
    if (next) {
      e.preventDefault();
      setTab(next);
      tabRefs.current[next]?.focus();
    }
  };

  return (
    <div className="space-y-4">
      <h1 className="text-2xl font-bold">{t('title')}</h1>
      <p className="text-muted text-sm">{t('description')}</p>

      <div
        role="tablist"
        aria-label={t('tabs.label')}
        className="inline-flex rounded-md border border-border bg-card text-sm overflow-hidden"
      >
        {TABS.map((k) => (
          <button
            key={k}
            ref={(el) => {
              tabRefs.current[k] = el;
            }}
            type="button"
            role="tab"
            aria-selected={tab === k}
            // Roving tabindex (WAI-ARIA Tabs pattern), matching Config.tsx;
            // arrow-key navigation handled by onKeyDown above.
            tabIndex={tab === k ? 0 : -1}
            onClick={() => setTab(k)}
            onKeyDown={onKeyDown}
            className={tab === k ? 'px-4 h-9 bg-accent/15 text-accent' : 'px-4 h-9 hover:bg-accent/5'}
          >
            {t(`tabs.${k}`)}
          </button>
        ))}
      </div>

      {tab === 'personal' && <PersonalTab />}
      {tab === 'server' && <ServerTab />}
    </div>
  );
}

/// Browser-local preferences. Nothing here touches the backend — the
/// banner spells that out so it's clearly distinct from the server tab.
function PersonalTab() {
  const { t, i18n } = useTranslation('settings');
  const { theme, setTheme } = useTheme();

  return (
    <div className="space-y-4">
      <p className="text-muted text-xs rounded-md border border-border bg-card px-3 py-2">
        {t('personal.scopeNote')}
      </p>
      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>{t('language.title')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2">
            <Label htmlFor="language-select">{t('language.label')}</Label>
            <Select
              id="language-select"
              value={i18n.resolvedLanguage ?? 'en'}
              onChange={(e) => {
                const code = e.target.value as LanguageCode;
                void i18n.changeLanguage(code);
              }}
            >
              {LANGUAGES.map((l) => (
                <option key={l.code} value={l.code}>
                  {l.label}
                </option>
              ))}
            </Select>
            <p className="text-muted text-xs">{t('language.persistedHint')}</p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>{t('theme.title')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2">
            <Label htmlFor="theme-select">{t('theme.label')}</Label>
            <Select
              id="theme-select"
              value={theme}
              onChange={(e) => {
                setTheme(e.target.value as Theme);
              }}
            >
              <option value="system">{t('theme.options.system')}</option>
              <option value="light">{t('theme.options.light')}</option>
              <option value="dark">{t('theme.options.dark')}</option>
            </Select>
            <p className="text-muted text-xs">{t('theme.persistedHint')}</p>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

/// Backend server settings (`server_settings` KV). Read by anyone
/// (viewer+), edited by operators. Saving here changes behaviour for the
/// whole deployment, not just this browser — the banner says so.
function ServerTab() {
  const { t } = useTranslation('settings');
  const { hasRole } = useAuth();
  const canOperate = hasRole('operator');
  const queryClient = useQueryClient();

  // Stored document (nullable fields) + the compiled-in defaults rendered
  // as faint placeholders — same pattern as the agent layered-config page
  // (Config.tsx fetches /api/config + /api/config/defaults).
  const settings = useQuery({
    queryKey: ['server-settings'],
    queryFn: () => apiFetch<ServerSettings>('/api/server-settings'),
    // Same guard as Config.tsx (#520): without it a background refetch
    // (window-focus etc.) re-fires the seeding effect below and clobbers
    // the operator's in-progress draft. The only refetch we want is our
    // own post-save invalidation.
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  const defaults = useQuery({
    queryKey: ['server-settings', 'defaults'],
    queryFn: () => apiFetch<ServerSettings>('/api/server-settings/defaults'),
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });

  // Local draft, kept as a string so the field can be blank. Blank =
  // unset (null), distinct from an explicit number. Seeded once the
  // stored value lands.
  const [pruneDays, setPruneDays] = useState('');
  // Trusted runner group for `tier: controller` jobs. Blank = unset
  // (controller-tier jobs run nowhere — fail-safe).
  const [controllerGroup, setControllerGroup] = useState('');
  useEffect(() => {
    if (settings.data) {
      setPruneDays(settings.data.agent_prune_days == null ? '' : String(settings.data.agent_prune_days));
      // Trim on seed so an unedited reload of a stored value with stray
      // whitespace doesn't read as dirty (the draft is compared trimmed).
      setControllerGroup((settings.data.controller_group ?? '').trim());
    }
  }, [settings.data]);

  const save = useMutation({
    mutationFn: (next: ServerSettings) =>
      apiFetch<ServerSettings>('/api/server-settings', {
        method: 'PUT',
        body: JSON.stringify(next),
      }),
    onSuccess: () => {
      toast.success(t('server.saved'));
      void queryClient.invalidateQueries({ queryKey: ['server-settings'] });
    },
    onError: (err) => toast.error(formatError(err)),
  });

  // Blank → unset (null). A non-blank value must be a whole number ≥ 1:
  // 0 / negatives are disallowed because "0 days" would mean "prune every
  // agent older than now" — to disable, clear the field instead.
  const trimmed = pruneDays.trim();
  const parsed = Number(trimmed);
  const pruneValue: number | null = trimmed === '' ? null : parsed;
  const pruneValid =
    trimmed === '' ||
    (Number.isInteger(parsed) && parsed >= 1 && parsed <= MAX_AGENT_PRUNE_DAYS);

  // controller_group: blank → unset (null). Any non-blank group name is
  // valid here; the backend resolves membership at dispatch time.
  const cgTrimmed = controllerGroup.trim();
  const controllerValue: string | null = cgTrimmed === '' ? null : cgTrimmed;

  // One save for the whole document (the PUT is a full replace). `dirty`
  // if either field diverges from the stored doc.
  const valid = pruneValid;
  const dirty =
    settings.data != null &&
    (pruneValue !== settings.data.agent_prune_days ||
      controllerValue !== (settings.data.controller_group ?? null));
  const doc: ServerSettings = {
    agent_prune_days: pruneValue,
    controller_group: controllerValue,
  };

  // Faint placeholder = what a blank field resolves to: the built-in
  // default if one exists, else a localised "(unset)" — mirrors the agent
  // config form's `ph()` helper. When a real default is introduced
  // backend-side it appears here automatically.
  const placeholder =
    defaults.data && defaults.data.agent_prune_days != null
      ? String(defaults.data.agent_prune_days)
      : t('server.agentPrune.unsetPlaceholder');

  return (
    <div className="space-y-4">
      <p className="text-muted text-xs rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2">
        {t('server.scopeNote')}
      </p>

      {settings.isError && (
        <p className="text-red-500 text-sm">{formatError(settings.error)}</p>
      )}

      <Card className="max-w-xl">
        <CardHeader>
          <CardTitle>{t('server.agentPrune.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-muted text-sm">{t('server.agentPrune.description')}</p>
          <div className="space-y-1">
            <Label htmlFor="agent-prune-days">{t('server.agentPrune.label')}</Label>
            <div className="flex items-center gap-2">
              <Input
                id="agent-prune-days"
                type="number"
                min={1}
                max={MAX_AGENT_PRUNE_DAYS}
                step={1}
                inputMode="numeric"
                value={pruneDays}
                placeholder={placeholder}
                disabled={!canOperate || settings.isLoading}
                onChange={(e) => setPruneDays(e.target.value)}
                className="w-32"
              />
              <span className="text-muted text-sm">{t('server.agentPrune.unit')}</span>
            </div>
            <p className="text-muted text-xs">{t('server.agentPrune.blankHint')}</p>
          </div>
        </CardContent>
      </Card>

      <Card className="max-w-xl">
        <CardHeader>
          <CardTitle>{t('server.controllerGroup.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-muted text-sm">{t('server.controllerGroup.description')}</p>
          <div className="space-y-1">
            <Label htmlFor="controller-group">{t('server.controllerGroup.label')}</Label>
            <Input
              id="controller-group"
              type="text"
              value={controllerGroup}
              placeholder={t('server.controllerGroup.unsetPlaceholder')}
              disabled={!canOperate || settings.isLoading}
              onChange={(e) => setControllerGroup(e.target.value)}
              className="w-64"
            />
            <p className="text-muted text-xs">{t('server.controllerGroup.blankHint')}</p>
          </div>
        </CardContent>
      </Card>

      <div className="flex items-center gap-3">
        <Button
          type="button"
          disabled={!canOperate || !valid || !dirty || save.isPending}
          title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
          onClick={() => save.mutate(doc)}
        >
          {save.isPending ? t('server.saving') : t('server.save')}
        </Button>
        {!canOperate && (
          <span className="text-xs text-muted">
            {t('rbac.operatorRequired', { ns: 'common' })}
          </span>
        )}
      </div>
    </div>
  );
}
