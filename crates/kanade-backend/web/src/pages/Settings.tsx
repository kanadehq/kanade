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
import {
  DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES,
  MAX_SUPPORT_UNLOCK_TTL_MINUTES,
  validateSupportCode,
  type SupportCode,
  type SupportCodeBody,
} from '@/lib/supportCodes';
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

// Mirrors `MAX_SESSION_TTL_HOURS` (kanade_shared::wire::server_settings):
// 365 days. The backend PUT enforces the same bound; this is client-side
// feedback only.
const MAX_SESSION_TTL_HOURS = 8_760;

// Mirrors `MAX_CHECK_STATUS_STALE_DAYS` (kanade_shared::wire::server_settings):
// 10 years. The backend PUT enforces the same bound; this is client-side
// feedback only. Unlike the others, 0 is VALID here (disables staleness).
const MAX_CHECK_STATUS_STALE_DAYS = 3650;

// Mirrors `MAX_OBJECT_STORE_CAP_MIB` (kanade_shared::wire::server_settings):
// 50 GiB — one bucket can never be configured to eat the whole JetStream
// file store. The backend PUT enforces the same bound.
const MAX_OBJECT_STORE_CAP_MIB = 51_200;

// Mirrors `MAX_OBJECT_STORE_TOTAL_MIB` (kanade_shared::wire::server_settings):
// the aggregate ceiling for the five effective caps — 50 GiB minus the
// streams' ~4.8 GiB reservations. The backend PUT enforces the same bound.
const MAX_OBJECT_STORE_TOTAL_MIB = 46_272;

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

/// Per-bucket disk caps (MiB) for the five NATS Object Stores (#1247) —
/// mirrors `kanade_shared::wire::ObjectStoreCaps`. Every field nullable:
/// `null` (or absent) ⇒ the bucket resolves to its built-in default.
interface ObjectStoreCaps {
  result_output_mib: number | null;
  agent_releases_mib: number | null;
  app_packages_mib: number | null;
  scripts_mib: number | null;
  collections_mib: number | null;
}

/// Backend-side server settings document (`server_settings` KV). Mirrors
/// `kanade_shared::wire::ServerSettings`: every field is nullable, where
/// `null` (or absent) means "unset — fall back to the built-in default".
/// The same shape is returned by `/api/server-settings/defaults` to carry
/// those built-in defaults (rendered as faint placeholders).
interface ServerSettings {
  agent_prune_days: number | null;
  collect_retention_days: number | null;
  session_ttl_hours: number | null;
  check_status_stale_days: number | null;
  controller_group: string | null;
  mail: MailSettings | null;
  object_store_caps: ObjectStoreCaps | null;
  /// Read-only here. Managed through its own endpoints (see
  /// [`ServerSettingsPatch`]) and absent from the document PUT entirely.
  /// `serde` omits the key when empty, so an older / never-configured
  /// deployment sends nothing — hence the optional.
  support_codes?: SupportCode[];
}

/// What the generic `PUT /api/server-settings` may carry. Deliberately
/// **excludes** `support_codes`: responses blank the stored hash, so a
/// round-tripped document would blank a live code if the merge accepted the
/// field. The backend ignores it either way, but keeping it out of the type
/// means the SPA can't even build such a body. Codes go through
/// `PUT`/`DELETE /api/server-settings/support-codes/{scope}` instead.
type ServerSettingsPatch = Omit<ServerSettings, 'support_codes'>;

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
  // Login-token lifetime (hours). Blank = unset, which falls back to the
  // built-in default (24 h, shown as the placeholder) — like collect
  // retention, blank does NOT disable sessions.
  const [sessionTtl, setSessionTtl] = useState('');
  // #1032②: days a check_status row may go stale before the Compliance page
  // hides it. Blank = unset (built-in 30d default); 0 disables staleness.
  const [staleDays, setStaleDays] = useState('');
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
  // Object-store disk caps (MiB), one string field per bucket (#1247).
  // All blank → whole key unset (every bucket on its built-in default);
  // a per-bucket blank falls back to that bucket's default.
  const [capResultOutput, setCapResultOutput] = useState('');
  const [capAgentReleases, setCapAgentReleases] = useState('');
  const [capAppPackages, setCapAppPackages] = useState('');
  const [capScripts, setCapScripts] = useState('');
  const [capCollections, setCapCollections] = useState('');
  useEffect(() => {
    if (settings.data) {
      setPruneDays(settings.data.agent_prune_days == null ? '' : String(settings.data.agent_prune_days));
      setCollectDays(
        settings.data.collect_retention_days == null
          ? ''
          : String(settings.data.collect_retention_days),
      );
      setSessionTtl(
        settings.data.session_ttl_hours == null ? '' : String(settings.data.session_ttl_hours),
      );
      setStaleDays(
        settings.data.check_status_stale_days == null
          ? ''
          : String(settings.data.check_status_stale_days),
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
      const c = settings.data.object_store_caps;
      setCapResultOutput(c?.result_output_mib == null ? '' : String(c.result_output_mib));
      setCapAgentReleases(c?.agent_releases_mib == null ? '' : String(c.agent_releases_mib));
      setCapAppPackages(c?.app_packages_mib == null ? '' : String(c.app_packages_mib));
      setCapScripts(c?.scripts_mib == null ? '' : String(c.scripts_mib));
      setCapCollections(c?.collections_mib == null ? '' : String(c.collections_mib));
    }
  }, [settings.data]);

  const save = useMutation({
    mutationFn: (next: ServerSettingsPatch) =>
      apiFetch<ServerSettings>('/api/server-settings', {
        method: 'PUT',
        body: JSON.stringify(next),
      }),
    onSuccess: () => {
      toast.success(t('server.saved'));
      // `exact: true` — React Query matches by PREFIX by default, so a bare
      // `['server-settings']` also invalidates `['server-settings','defaults']`
      // (static, compiled-in) and `['server-settings','support-codes']` (which
      // this save cannot have changed: the document merge never touches that
      // key). Both refetches can only return what the cache already holds, and
      // the roster one contradicts the isolation its own comment promises.
      void queryClient.invalidateQueries({ queryKey: ['server-settings'], exact: true });
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

  // session_ttl_hours: blank → unset (null → built-in 24h default). A
  // non-blank value must be a whole number in [1, MAX_SESSION_TTL_HOURS].
  const sessionTtlTrimmed = sessionTtl.trim();
  const sessionTtlParsed = Number(sessionTtlTrimmed);
  const sessionTtlValue: number | null = sessionTtlTrimmed === '' ? null : sessionTtlParsed;
  const sessionTtlValid =
    sessionTtlTrimmed === '' ||
    (Number.isInteger(sessionTtlParsed) &&
      sessionTtlParsed >= 1 &&
      sessionTtlParsed <= MAX_SESSION_TTL_HOURS);

  // check_status_stale_days: blank → unset (null → built-in 30d default). A
  // non-blank value must be a whole number in [0, MAX]. Unlike the others, 0
  // is VALID — it disables staleness (every row shown).
  const staleTrimmed = staleDays.trim();
  const staleParsed = Number(staleTrimmed);
  const staleValue: number | null = staleTrimmed === '' ? null : staleParsed;
  const staleValid =
    staleTrimmed === '' ||
    (Number.isInteger(staleParsed) &&
      staleParsed >= 0 &&
      staleParsed <= MAX_CHECK_STATUS_STALE_DAYS);

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

  // object_store_caps (#1247): per-bucket blank → that bucket's built-in
  // default; ALL blank → whole key unset. A non-blank value must be a
  // whole number in [1, MAX_OBJECT_STORE_CAP_MIB] (0 is rejected because
  // NATS reads max_bytes: 0 as UNLIMITED — the failure mode this removes).
  const capInputs: { key: keyof ObjectStoreCaps; raw: string }[] = [
    { key: 'result_output_mib', raw: capResultOutput },
    { key: 'agent_releases_mib', raw: capAgentReleases },
    { key: 'app_packages_mib', raw: capAppPackages },
    { key: 'scripts_mib', raw: capScripts },
    { key: 'collections_mib', raw: capCollections },
  ];
  const capsValid = capInputs.every(({ raw }) => {
    const t = raw.trim();
    if (t === '') return true;
    const n = Number(t);
    return Number.isInteger(n) && n >= 1 && n <= MAX_OBJECT_STORE_CAP_MIB;
  });
  // Aggregate budget: the effective total (entered values + built-in
  // defaults for blanks) must fit the broker-wide max_file_store, or the
  // broker refuses every cap update (10047) and the saved document lies.
  // The backend PUT enforces this authoritatively; this is early feedback.
  // Skipped while /defaults hasn't loaded (per-field check above still
  // applies).
  const capsEffectiveTotal = capInputs.reduce((sum, { key, raw }) => {
    const t = raw.trim();
    if (t !== '') {
      const n = Number(t);
      return sum + (Number.isInteger(n) ? n : 0);
    }
    return sum + (defaults.data?.object_store_caps?.[key] ?? 0);
  }, 0);
  const capsAggregateValid =
    defaults.data == null || capsEffectiveTotal <= MAX_OBJECT_STORE_TOTAL_MIB;
  const capsAllBlank = capInputs.every(({ raw }) => raw.trim() === '');
  const capsValue: ObjectStoreCaps | null = capsAllBlank
    ? null
    : (Object.fromEntries(
        capInputs.map(({ key, raw }) => {
          const t = raw.trim();
          return [key, t === '' ? null : Number(t)];
        }),
      ) as unknown as ObjectStoreCaps);
  const storedCaps = settings.data?.object_store_caps ?? null;
  const capsDirty =
    (capsValue === null) !== (storedCaps === null) ||
    (capsValue !== null &&
      storedCaps !== null &&
      capInputs.some(({ key }) => (capsValue[key] ?? null) !== (storedCaps[key] ?? null)));

  // One save for the whole document. The PUT merges per-field, but the SPA
  // always sends every field it knows, so an unchanged one is re-sent
  // as-is. `dirty` if any field diverges from the stored doc.
  const valid =
    pruneValid &&
    collectValid &&
    sessionTtlValid &&
    staleValid &&
    mailValid &&
    capsValid &&
    capsAggregateValid;
  const dirty =
    settings.data != null &&
    (pruneValue !== settings.data.agent_prune_days ||
      collectValue !== settings.data.collect_retention_days ||
      sessionTtlValue !== settings.data.session_ttl_hours ||
      staleValue !== settings.data.check_status_stale_days ||
      controllerValue !== (settings.data.controller_group ?? null) ||
      mailDirty ||
      capsDirty);
  const doc: ServerSettingsPatch = {
    agent_prune_days: pruneValue,
    collect_retention_days: collectValue,
    session_ttl_hours: sessionTtlValue,
    check_status_stale_days: staleValue,
    controller_group: controllerValue,
    mail: mailValue,
    object_store_caps: capsValue,
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
  // session_ttl_hours also has a real built-in default (24h), so the
  // placeholder shows that number; a blank field resolves to it.
  const sessionTtlPlaceholder =
    defaults.data && defaults.data.session_ttl_hours != null
      ? String(defaults.data.session_ttl_hours)
      : t('server.sessionTtl.unsetPlaceholder');
  // check_status_stale_days has a real built-in default (30 d); a blank field
  // resolves to it.
  const stalePlaceholder =
    defaults.data && defaults.data.check_status_stale_days != null
      ? String(defaults.data.check_status_stale_days)
      : t('server.checkStale.unsetPlaceholder');
  // Every object-store cap has a real built-in default (#1247), so each
  // blank field resolves to the number shown faintly.
  const capPlaceholder = (key: keyof ObjectStoreCaps): string => {
    const v = defaults.data?.object_store_caps?.[key];
    return v != null ? String(v) : t('server.objectStoreCaps.unsetPlaceholder');
  };

  return (
    <div className="space-y-4">
      <p className="text-muted text-xs rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2">
        {t('server.scopeNote')}
      </p>

      {settings.isError && (
        <p className="text-red-500 text-sm">{formatError(settings.error)}</p>
      )}

      {/* The four single-number knobs pair up two-across: each is a label and a
          narrow field, so a full-width row wasted most of it and pushed the
          fields that matter far apart vertically. The taller cards below stay
          one-per-row — and everything stays ABOVE the single 保存 button, which
          is the cue for what that button covers. */}
      <div className="grid gap-4 lg:grid-cols-2 max-w-4xl">
        <Card>
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

        <Card>
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

        <Card>
          <CardHeader>
            <CardTitle>{t('server.objectStoreCaps.title')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <p className="text-muted text-sm">{t('server.objectStoreCaps.description')}</p>
            {(
              [
                ['result_output_mib', capResultOutput, setCapResultOutput],
                ['agent_releases_mib', capAgentReleases, setCapAgentReleases],
                ['app_packages_mib', capAppPackages, setCapAppPackages],
                ['scripts_mib', capScripts, setCapScripts],
                ['collections_mib', capCollections, setCapCollections],
              ] as const
            ).map(([key, value, setValue]) => (
              <div className="space-y-1" key={key}>
                <Label htmlFor={`cap-${key}`}>{t(`server.objectStoreCaps.fields.${key}`)}</Label>
                <div className="flex items-center gap-2">
                  <Input
                    id={`cap-${key}`}
                    type="number"
                    min={1}
                    max={MAX_OBJECT_STORE_CAP_MIB}
                    step={1}
                    inputMode="numeric"
                    value={value}
                    placeholder={capPlaceholder(key)}
                    disabled={!canOperate || settings.isLoading}
                    onChange={(e) => setValue(e.target.value)}
                    className="w-32"
                  />
                  <span className="text-muted text-sm">{t('server.objectStoreCaps.unit')}</span>
                </div>
              </div>
            ))}
            <p className="text-muted text-xs">{t('server.objectStoreCaps.blankHint')}</p>
            {!capsAggregateValid && (
              <p className="text-danger text-xs">
                {t('server.objectStoreCaps.overBudget', {
                  total: capsEffectiveTotal,
                  max: MAX_OBJECT_STORE_TOTAL_MIB,
                })}
              </p>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>{t('server.sessionTtl.title')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <p className="text-muted text-sm">{t('server.sessionTtl.description')}</p>
            <div className="space-y-1">
              <Label htmlFor="session-ttl-hours">{t('server.sessionTtl.label')}</Label>
              <div className="flex items-center gap-2">
                <Input
                  id="session-ttl-hours"
                  type="number"
                  min={1}
                  max={MAX_SESSION_TTL_HOURS}
                  step={1}
                  inputMode="numeric"
                  value={sessionTtl}
                  placeholder={sessionTtlPlaceholder}
                  disabled={!canOperate || settings.isLoading}
                  onChange={(e) => setSessionTtl(e.target.value)}
                  className="w-32"
                />
                <span className="text-muted text-sm">{t('server.sessionTtl.unit')}</span>
              </div>
              <p className="text-muted text-xs">{t('server.sessionTtl.blankHint')}</p>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>{t('server.checkStale.title')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <p className="text-muted text-sm">{t('server.checkStale.description')}</p>
            <div className="space-y-1">
              <Label htmlFor="check-stale-days">{t('server.checkStale.label')}</Label>
              <div className="flex items-center gap-2">
                <Input
                  id="check-stale-days"
                  type="number"
                  min={0}
                  max={MAX_CHECK_STATUS_STALE_DAYS}
                  step={1}
                  inputMode="numeric"
                  value={staleDays}
                  placeholder={stalePlaceholder}
                  disabled={!canOperate || settings.isLoading}
                  onChange={(e) => setStaleDays(e.target.value)}
                  className="w-32"
                />
                <span className="text-muted text-sm">{t('server.checkStale.unit')}</span>
              </div>
              <p className="text-muted text-xs">{t('server.checkStale.blankHint')}</p>
            </div>
          </CardContent>
        </Card>

      </div>

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

      <Card className="max-w-3xl">
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
        {/* Deliberately BEFORE the support-codes card in the DOM: that card
            saves through its own endpoint, so it must not look like it is
            covered by this button. */}
        {!canOperate && (
          <span className="text-xs text-muted">
            {t('rbac.operatorRequired', { ns: 'common' })}
          </span>
        )}
      </div>

      <SupportCodesCard />

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

/// Support-code roster + editor — the operator side of the Client App's
/// helpdesk unlock (`client.unlock`, #1166).
///
/// Its own card AND its own save button, deliberately outside the document
/// form above, because a secret behaves differently from every other field on
/// this page:
///
/// - **Write-only.** `GET /api/server-settings` blanks the stored argon2id
///   hash, so there is nothing to seed an "edit" field with. An entry's mere
///   presence is what tells us a scope has a code; rotating means typing a new
///   one, never reading the old one back.
/// - **Not part of the document merge.** Routing it through the generic PUT
///   would let a redacted document round-trip and blank a live code, so the
///   backend gives it dedicated endpoints and this component uses them.
function SupportCodesCard() {
  const { t } = useTranslation('settings');
  const { hasRole } = useAuth();
  const canOperate = hasRole('operator');
  const queryClient = useQueryClient();
  const confirm = useConfirm();

  // The roster gets its OWN cache entry rather than reading the document
  // query the form above seeds from. Both come from `GET /api/server-settings`,
  // so this costs one extra GET on mount — worth it, because writing a code
  // must not disturb the other card: the document query's `staleTime: Infinity`
  // exists precisely so nothing but its own save refetches it (#520), and any
  // refetch re-fires its seeding effect and clobbers an in-progress draft.
  // Sharing the key would have made "save a support code" quietly reset every
  // other field the operator was editing.
  //
  // The isolation runs BOTH ways, and neither direction is free: writes here
  // never invalidate (they adopt the mutation response instead), and the
  // document save passes `exact: true` so its invalidation doesn't
  // prefix-match this key.
  const roster = useQuery({
    queryKey: ['server-settings', 'support-codes'],
    queryFn: async () =>
      (await apiFetch<ServerSettings>('/api/server-settings')).support_codes ?? [],
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  // Sorted by scope, not by write order. The endpoint's upsert removes the
  // old entry and pushes the new one, so a rotate would otherwise move that
  // row to the bottom of the list — rows jumping under the operator's cursor
  // right after they act on one is how a 削除 lands on the wrong scope.
  const codes = [...(roster.data ?? [])].sort((a, b) => a.scope.localeCompare(b.scope));

  // Both endpoints answer with the merged, redacted document, so the roster
  // is updated from the response instead of invalidating and re-fetching.
  const adoptRoster = (next: ServerSettings) =>
    queryClient.setQueryData<SupportCode[]>(
      ['server-settings', 'support-codes'],
      next.support_codes ?? [],
    );

  const [scope, setScope] = useState('');
  const [code, setCode] = useState('');
  const [label, setLabel] = useState('');
  const [ttlMinutes, setTtlMinutes] = useState('');
  const [disabled, setDisabled] = useState(false);

  const clearDraft = () => {
    setScope('');
    setCode('');
    setLabel('');
    setTtlMinutes('');
    setDisabled(false);
    // Re-arm the seeding effect: after a save the roster changed, so typing
    // the same scope again must re-read the (possibly new) stored values.
    seededScope.current = null;
  };

  const upsert = useMutation({
    mutationFn: ({ scope: s, body }: { scope: string; body: SupportCodeBody }) =>
      apiFetch<ServerSettings>(
        `/api/server-settings/support-codes/${encodeURIComponent(s)}`,
        { method: 'PUT', body: JSON.stringify(body) },
      ),
    onSuccess: (next) => {
      toast.success(t('server.supportCodes.saved'));
      // Blank the draft on success so the plaintext doesn't linger in a
      // form field (and in React state) after it has been committed.
      clearDraft();
      adoptRoster(next);
    },
    onError: (err) => toast.error(formatError(err)),
  });

  const remove = useMutation({
    mutationFn: (s: string) =>
      apiFetch<ServerSettings>(
        `/api/server-settings/support-codes/${encodeURIComponent(s)}`,
        { method: 'DELETE' },
      ),
    onSuccess: (next) => {
      toast.success(t('server.supportCodes.deleted'));
      adoptRoster(next);
    },
    onError: (err) => toast.error(formatError(err)),
  });

  const error = validateSupportCode({ scope, code, label, ttlMinutes });
  // Show the message only once it's about something the operator has actually
  // filled in. An empty draft isn't "invalid", it's just not started — and
  // `codeShort` on an untouched code field would scold them for not having
  // reached it yet, which is what typing only the scope used to produce. The
  // button stays disabled on `error` regardless, so nothing is submittable
  // just because the message is hidden.
  const started = scope !== '' || code !== '' || label !== '' || ttlMinutes !== '';
  const showError = started && error != null && !(error === 'codeShort' && code === '');
  const existing = codes.find((c) => c.scope === scope.trim());
  const rotating = existing != null;
  // With no roster, `codes` is empty and every scope looks new — so a submit
  // would skip the rotate confirmation and silently replace a live code whose
  // previous secret is then unrecoverable. We can't tell create from rotate,
  // so we don't write at all. (The GET that failed and the PUT share a host,
  // so this is rarely a real loss of capability; reloading is the fix.)
  const rosterUnknown = roster.isError || roster.isLoading;

  // Seed the non-secret fields from the matched entry the moment the typed
  // scope becomes an existing one. A submit REPLACES the whole entry, so
  // without this a rotate — where the operator only means to change the
  // secret — silently wipes the label and TTL they had configured. These
  // fields are not secrets, so unlike the code they can be read back.
  //
  // Keyed on the seeded scope rather than on `existing`, so re-seeding
  // happens once per scope change and never overwrites edits the operator
  // makes afterwards (including deliberately blanking the label).
  const seededScope = useRef<string | null>(null);
  useEffect(() => {
    const s = scope.trim();
    if (existing == null) {
      // Leaving a matched scope arms the next match to re-seed.
      if (seededScope.current !== null && seededScope.current !== s) {
        seededScope.current = null;
      }
      return;
    }
    if (seededScope.current === s) return;
    seededScope.current = s;
    setLabel(existing.label ?? '');
    setTtlMinutes(existing.ttl_minutes == null ? '' : String(existing.ttl_minutes));
    setDisabled(existing.disabled ?? false);
  }, [scope, existing]);

  const onSubmit = async () => {
    const s = scope.trim();
    // Rotating is destructive in the one way that matters: the previous
    // secret becomes unrecoverable, and any desk still carrying it is locked
    // out with no warning. Confirm it; creating a new scope needs no prompt.
    if (rotating) {
      const ok = await confirm({
        title: t('server.supportCodes.rotateConfirm.title'),
        description: t('server.supportCodes.rotateConfirm.description', { scope: s }),
        confirmLabel: t('server.supportCodes.rotateConfirm.confirm'),
        danger: true,
      });
      if (!ok) return;
    }
    upsert.mutate({
      scope: s,
      body: {
        code,
        label: label.trim() === '' ? null : label.trim(),
        ttl_minutes: ttlMinutes.trim() === '' ? null : Number(ttlMinutes),
        disabled,
      },
    });
  };

  const onDelete = async (s: string) => {
    const ok = await confirm({
      title: t('server.supportCodes.deleteConfirm.title'),
      description: t('server.supportCodes.deleteConfirm.description', { scope: s }),
      confirmLabel: t('server.supportCodes.deleteConfirm.confirm'),
      danger: true,
    });
    if (ok) remove.mutate(s);
  };

  return (
    <Card className="max-w-3xl">
      <CardHeader>
        <CardTitle>{t('server.supportCodes.title')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        <p className="text-muted text-sm">{t('server.supportCodes.description')}</p>
        <p className="text-muted text-xs rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2">
          {t('server.supportCodes.scopeNote')}
        </p>

        {/* Roster. No hash, no code — only which scopes exist and how they
            behave, which is everything the operator can act on.
            `isError` is checked FIRST: on a failed fetch `data` stays
            undefined, so without this branch a fetch failure renders as
            「コードが設定されていません」— an outright false statement about
            fleet-wide state. */}
        {roster.isError ? (
          <p className="text-red-500 text-sm">{formatError(roster.error)}</p>
        ) : roster.isLoading ? (
          <p className="text-muted text-sm">{t('server.supportCodes.loading')}</p>
        ) : codes.length === 0 ? (
          <p className="text-muted text-sm">{t('server.supportCodes.empty')}</p>
        ) : (
          <ul className="divide-y divide-border rounded-md border border-border">
            {codes.map((c) => (
              <li key={c.scope} className="flex items-center gap-3 px-3 py-2 text-sm">
                <span className="font-mono">{c.scope}</span>
                {c.label && <span className="text-muted">{c.label}</span>}
                <span className="text-muted text-xs">
                  {t('server.supportCodes.ttlSummary', {
                    minutes: c.ttl_minutes ?? DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES,
                  })}
                </span>
                {c.disabled && (
                  <span className="text-xs rounded-full border border-amber-500/40 bg-amber-500/10 px-2 py-0.5">
                    {t('server.supportCodes.disabledBadge')}
                  </span>
                )}
                <Button
                  type="button"
                  variant="danger"
                  className="ml-auto"
                  disabled={!canOperate || remove.isPending}
                  title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
                  // Every row's button reads just "削除" otherwise, which is
                  // ambiguous for anyone navigating by control rather than by
                  // row text — and the action is irreversible.
                  aria-label={`${t('server.supportCodes.delete')} ${c.scope}`}
                  onClick={() => void onDelete(c.scope)}
                >
                  {t('server.supportCodes.delete')}
                </Button>
              </li>
            ))}
          </ul>
        )}

        {/* Editor. Upsert by scope — typing an existing scope rotates it. */}
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="space-y-1">
            <Label htmlFor="support-code-scope">{t('server.supportCodes.scope')}</Label>
            <Input
              id="support-code-scope"
              type="text"
              value={scope}
              placeholder="support"
              disabled={!canOperate}
              onChange={(e) => setScope(e.target.value)}
              autoComplete="off"
              className="font-mono"
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="support-code-label">{t('server.supportCodes.label')}</Label>
            <Input
              id="support-code-label"
              type="text"
              value={label}
              placeholder={t('server.supportCodes.labelPlaceholder')}
              disabled={!canOperate}
              onChange={(e) => setLabel(e.target.value)}
              autoComplete="off"
            />
          </div>
          <div className="space-y-1 sm:col-span-2">
            <Label htmlFor="support-code-secret">{t('server.supportCodes.code')}</Label>
            <Input
              id="support-code-secret"
              type="password"
              value={code}
              disabled={!canOperate}
              onChange={(e) => setCode(e.target.value)}
              // `new-password` (not `off`): stops the browser offering a
              // saved credential for this origin, and stops it offering to
              // save the support code as one.
              autoComplete="new-password"
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="support-code-ttl">{t('server.supportCodes.ttl')}</Label>
            <div className="flex items-center gap-2">
              <Input
                id="support-code-ttl"
                type="number"
                min={1}
                max={MAX_SUPPORT_UNLOCK_TTL_MINUTES}
                step={1}
                inputMode="numeric"
                value={ttlMinutes}
                placeholder={String(DEFAULT_SUPPORT_UNLOCK_TTL_MINUTES)}
                disabled={!canOperate}
                onChange={(e) => setTtlMinutes(e.target.value)}
                className="w-28"
              />
              <span className="text-muted text-sm">{t('server.supportCodes.ttlUnit')}</span>
            </div>
          </div>
          <div className="space-y-1">
            <Label htmlFor="support-code-disabled">{t('server.supportCodes.disabled')}</Label>
            <div className="flex items-center gap-2 pt-2">
              <input
                id="support-code-disabled"
                type="checkbox"
                checked={disabled}
                disabled={!canOperate}
                onChange={(e) => setDisabled(e.target.checked)}
              />
              <span className="text-muted text-xs">{t('server.supportCodes.disabledHint')}</span>
            </div>
          </div>
        </div>

        <p className="text-muted text-xs">{t('server.supportCodes.writeOnlyHint')}</p>
        {showError && (
          <p className="text-red-500 text-xs">{t(`server.supportCodes.errors.${error}`)}</p>
        )}

        <div className="flex items-center gap-3">
          <Button
            type="button"
            disabled={!canOperate || error != null || rosterUnknown || upsert.isPending}
            title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
            onClick={() => void onSubmit()}
          >
            {upsert.isPending
              ? t('server.supportCodes.saving')
              : rotating
                ? t('server.supportCodes.rotate')
                : t('server.supportCodes.create')}
          </Button>
          {!canOperate && (
            <span className="text-xs text-muted">
              {t('rbac.operatorRequired', { ns: 'common' })}
            </span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
