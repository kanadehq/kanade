import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
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

// Mirrors `MAX_COLLECT_RETENTION_DAYS` (kanade_shared::wire::server_settings):
// 10 years. The backend PUT enforces the same bound; this is client-side
// feedback only.
const MAX_COLLECT_RETENTION_DAYS = 3650;

/// SMTP transport security — mirrors `kanade_shared::config::MailEncryption`
/// (serialised lowercase).
type MailEncryption = 'starttls' | 'tls' | 'none';

/// Non-secret SMTP relay settings — mirrors `kanade_shared::config::
/// MailSection`. The password is NOT here: it stays a server-side secret
/// (`MailPassword` registry value / `$KANADE_MAIL_PASSWORD`).
interface MailSettings {
  host: string;
  port: number;
  encryption: MailEncryption;
  from: string;
  username: string | null;
}

/// Backend-side server settings document (`server_settings` KV). Mirrors
/// `kanade_shared::wire::ServerSettings`: every field is nullable, where
/// `null` (or absent) means "unset — fall back to the built-in default".
/// The same shape is returned by `/api/server-settings/defaults` to carry
/// those built-in defaults (rendered as faint placeholders).
interface ServerSettings {
  agent_prune_days: number | null;
  collect_retention_days: number | null;
  controller_group: string | null;
  mail: MailSettings | null;
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
  const confirm = useConfirm();
  // Pending "reload once the backend is back" poll timer, tracked so it can
  // be cancelled if this tab unmounts mid-restart (else the reload would fire
  // later and blow away whatever the user navigated to).
  const restartTimer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (restartTimer.current != null) window.clearTimeout(restartTimer.current);
    },
    [],
  );

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
  // Collect-bundle retention window (days). Blank = unset, which falls back
  // to the built-in default (30 d, shown as the placeholder) — unlike the
  // prune field, blank here does NOT disable retention.
  const [collectDays, setCollectDays] = useState('');
  // Trusted runner group for `tier: controller` jobs. Blank = unset
  // (controller-tier jobs run nowhere — fail-safe).
  const [controllerGroup, setControllerGroup] = useState('');
  // Mail (SMTP) settings, kept as separate string fields so each can be
  // blank. Mail is "configured" once host + from are filled; clearing them
  // unsets it (email becomes a no-op). Changes apply on the next backend
  // restart — the banner in the card says so.
  const [mailHost, setMailHost] = useState('');
  const [mailPort, setMailPort] = useState('');
  const [mailEncryption, setMailEncryption] = useState<MailEncryption>('starttls');
  const [mailFrom, setMailFrom] = useState('');
  const [mailUsername, setMailUsername] = useState('');
  useEffect(() => {
    if (settings.data) {
      setPruneDays(settings.data.agent_prune_days == null ? '' : String(settings.data.agent_prune_days));
      setCollectDays(
        settings.data.collect_retention_days == null
          ? ''
          : String(settings.data.collect_retention_days),
      );
      // Trim on seed so an unedited reload of a stored value with stray
      // whitespace doesn't read as dirty (the draft is compared trimmed).
      setControllerGroup((settings.data.controller_group ?? '').trim());
      const m = settings.data.mail;
      setMailHost((m?.host ?? '').trim());
      setMailPort(m == null ? '' : String(m.port));
      setMailEncryption(m?.encryption ?? 'starttls');
      setMailFrom((m?.from ?? '').trim());
      setMailUsername((m?.username ?? '').trim());
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

  // Restart the backend service (operator). The server exits and its SCM
  // recovery actions relaunch it (~5 s), so the API is briefly unavailable.
  // Rather than a blind timed reload (which could land on a dead server if
  // startup runs long), poll the public `/health` liveness probe and reload
  // the moment it answers — capped so a dev/console backend that doesn't
  // auto-restart doesn't poll forever.
  const restart = useMutation({
    mutationFn: () => apiFetch<{ status: string }>('/api/server/restart', { method: 'POST' }),
    onSuccess: () => {
      toast.success(t('server.restart.toast'));
      let attempts = 0;
      const maxAttempts = 60; // ~1 min of retries after the process exits
      const poll = () => {
        fetch('/health', { cache: 'no-store' })
          .then((res) => {
            if (res.ok) window.location.reload();
            else if (attempts++ < maxAttempts) restartTimer.current = window.setTimeout(poll, 1000);
          })
          .catch(() => {
            // Server still down (connection refused) — keep waiting.
            if (attempts++ < maxAttempts) restartTimer.current = window.setTimeout(poll, 1000);
          });
      };
      // Wait past the handler's exit grace before the first probe. Tracked in
      // restartTimer so the unmount cleanup above can cancel a pending reload.
      restartTimer.current = window.setTimeout(poll, 2000);
    },
    onError: (err) => toast.error(formatError(err)),
  });
  const onRestartClick = async () => {
    if (
      await confirm({
        title: t('server.restart.confirmTitle'),
        description: t('server.restart.confirmDescription'),
        confirmLabel: t('server.restart.confirmLabel'),
        danger: true,
      })
    ) {
      restart.mutate();
    }
  };

  // Blank → unset (null). A non-blank value must be a whole number ≥ 1:
  // 0 / negatives are disallowed because "0 days" would mean "prune every
  // agent older than now" — to disable, clear the field instead.
  const trimmed = pruneDays.trim();
  const parsed = Number(trimmed);
  const pruneValue: number | null = trimmed === '' ? null : parsed;
  const pruneValid =
    trimmed === '' ||
    (Number.isInteger(parsed) && parsed >= 1 && parsed <= MAX_AGENT_PRUNE_DAYS);

  // collect_retention_days: blank → unset (null → built-in default). A
  // non-blank value must be a whole number in [1, MAX_COLLECT_RETENTION_DAYS].
  const collectTrimmed = collectDays.trim();
  const collectParsed = Number(collectTrimmed);
  const collectValue: number | null = collectTrimmed === '' ? null : collectParsed;
  const collectValid =
    collectTrimmed === '' ||
    (Number.isInteger(collectParsed) &&
      collectParsed >= 1 &&
      collectParsed <= MAX_COLLECT_RETENTION_DAYS);

  // controller_group: blank → unset (null). Any non-blank group name is
  // valid here; the backend resolves membership at dispatch time.
  const cgTrimmed = controllerGroup.trim();
  const controllerValue: string | null = cgTrimmed === '' ? null : cgTrimmed;

  // mail: "configured" once host OR from is filled; both blank → unset
  // (null). A partially-filled config is invalid — the Save button stays
  // disabled until it's either complete or fully cleared.
  const mailHostTrimmed = mailHost.trim();
  const mailFromTrimmed = mailFrom.trim();
  const mailUserTrimmed = mailUsername.trim();
  const mailPortTrimmed = mailPort.trim();
  const mailConfigured = mailHostTrimmed !== '' || mailFromTrimmed !== '';
  const mailPortNum = Number(mailPortTrimmed);
  const mailValue: MailSettings | null = mailConfigured
    ? {
        host: mailHostTrimmed,
        port: mailPortNum,
        encryption: mailEncryption,
        from: mailFromTrimmed,
        username: mailUserTrimmed === '' ? null : mailUserTrimmed,
      }
    : null;
  // Valid when unset, or when host + a plausible from-address + an in-range
  // port are all present. The backend re-validates `from` with the real
  // parser; this just gives immediate feedback.
  const mailPortValid =
    mailPortTrimmed !== '' &&
    Number.isInteger(mailPortNum) &&
    mailPortNum >= 1 &&
    mailPortNum <= 65535;
  const mailValid =
    !mailConfigured ||
    (mailHostTrimmed !== '' && mailFromTrimmed.includes('@') && mailPortValid);

  const storedMail = settings.data?.mail ?? null;
  const mailDirty =
    (mailValue === null) !== (storedMail === null) ||
    (mailValue !== null &&
      storedMail !== null &&
      (mailValue.host !== storedMail.host ||
        mailValue.port !== storedMail.port ||
        mailValue.encryption !== storedMail.encryption ||
        mailValue.from !== storedMail.from ||
        (mailValue.username ?? null) !== (storedMail.username ?? null)));

  // One save for the whole document. The PUT merges per-field, but the SPA
  // always sends every field it knows, so an unchanged one is re-sent
  // as-is. `dirty` if any field diverges from the stored doc.
  const valid = pruneValid && collectValid && mailValid;
  const dirty =
    settings.data != null &&
    (pruneValue !== settings.data.agent_prune_days ||
      collectValue !== settings.data.collect_retention_days ||
      controllerValue !== (settings.data.controller_group ?? null) ||
      mailDirty);
  const doc: ServerSettings = {
    agent_prune_days: pruneValue,
    collect_retention_days: collectValue,
    controller_group: controllerValue,
    mail: mailValue,
  };

  // Faint placeholder = what a blank field resolves to: the built-in
  // default if one exists, else a localised "(unset)" — mirrors the agent
  // config form's `ph()` helper. When a real default is introduced
  // backend-side it appears here automatically.
  const placeholder =
    defaults.data && defaults.data.agent_prune_days != null
      ? String(defaults.data.agent_prune_days)
      : t('server.agentPrune.unsetPlaceholder');
  // collect_retention_days has a real built-in default (30 d), so the
  // placeholder always shows a number — a blank field resolves to it.
  const collectPlaceholder =
    defaults.data && defaults.data.collect_retention_days != null
      ? String(defaults.data.collect_retention_days)
      : t('server.collectRetention.unsetPlaceholder');

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
          <CardTitle>{t('server.collectRetention.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-muted text-sm">{t('server.collectRetention.description')}</p>
          <div className="space-y-1">
            <Label htmlFor="collect-retention-days">{t('server.collectRetention.label')}</Label>
            <div className="flex items-center gap-2">
              <Input
                id="collect-retention-days"
                type="number"
                min={1}
                max={MAX_COLLECT_RETENTION_DAYS}
                step={1}
                inputMode="numeric"
                value={collectDays}
                placeholder={collectPlaceholder}
                disabled={!canOperate || settings.isLoading}
                onChange={(e) => setCollectDays(e.target.value)}
                className="w-32"
              />
              <span className="text-muted text-sm">{t('server.collectRetention.unit')}</span>
            </div>
            <p className="text-muted text-xs">{t('server.collectRetention.blankHint')}</p>
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

      <Card className="max-w-xl">
        <CardHeader>
          <CardTitle>{t('server.mail.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-muted text-sm">{t('server.mail.description')}</p>
          <p className="text-muted text-xs rounded-md border border-border bg-card px-3 py-2">
            {t('server.mail.restartHint')}
          </p>
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="space-y-1">
              <Label htmlFor="mail-host">{t('server.mail.host')}</Label>
              <Input
                id="mail-host"
                type="text"
                value={mailHost}
                placeholder="smtp.example.com"
                disabled={!canOperate || settings.isLoading}
                onChange={(e) => setMailHost(e.target.value)}
              />
            </div>
            <div className="space-y-1">
              <Label htmlFor="mail-port">{t('server.mail.port')}</Label>
              <Input
                id="mail-port"
                type="number"
                min={1}
                max={65535}
                step={1}
                inputMode="numeric"
                value={mailPort}
                placeholder="587"
                disabled={!canOperate || settings.isLoading}
                onChange={(e) => setMailPort(e.target.value)}
              />
            </div>
            <div className="space-y-1">
              <Label htmlFor="mail-encryption">{t('server.mail.encryption')}</Label>
              <Select
                id="mail-encryption"
                value={mailEncryption}
                disabled={!canOperate || settings.isLoading}
                onChange={(e) => setMailEncryption(e.target.value as MailEncryption)}
              >
                <option value="starttls">{t('server.mail.encryptionOptions.starttls')}</option>
                <option value="tls">{t('server.mail.encryptionOptions.tls')}</option>
                <option value="none">{t('server.mail.encryptionOptions.none')}</option>
              </Select>
            </div>
            <div className="space-y-1">
              <Label htmlFor="mail-from">{t('server.mail.from')}</Label>
              <Input
                id="mail-from"
                type="email"
                value={mailFrom}
                placeholder="kanade-noreply@example.com"
                disabled={!canOperate || settings.isLoading}
                onChange={(e) => setMailFrom(e.target.value)}
              />
            </div>
            <div className="space-y-1 sm:col-span-2">
              <Label htmlFor="mail-username">{t('server.mail.username')}</Label>
              <Input
                id="mail-username"
                type="text"
                value={mailUsername}
                placeholder={t('server.mail.usernamePlaceholder')}
                disabled={!canOperate || settings.isLoading}
                onChange={(e) => setMailUsername(e.target.value)}
                autoComplete="off"
              />
            </div>
          </div>
          <p className="text-muted text-xs">{t('server.mail.passwordHint')}</p>
          <p className="text-muted text-xs">{t('server.mail.blankHint')}</p>
          {mailConfigured && !mailValid && (
            <p className="text-red-500 text-xs">{t('server.mail.invalid')}</p>
          )}
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

      <Card className="max-w-xl border-danger/40">
        <CardHeader>
          <CardTitle>{t('server.restart.title')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-muted text-sm">{t('server.restart.description')}</p>
          <div className="flex items-center gap-3">
            <Button
              type="button"
              variant="danger"
              // Stay disabled through `isSuccess` too: after the mutation
              // resolves the process is exiting and we're polling to reload,
              // so the button must not flash back to clickable (double-submit).
              disabled={!canOperate || restart.isPending || restart.isSuccess}
              title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
              onClick={onRestartClick}
            >
              {restart.isPending || restart.isSuccess
                ? t('server.restart.restarting')
                : t('server.restart.button')}
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
