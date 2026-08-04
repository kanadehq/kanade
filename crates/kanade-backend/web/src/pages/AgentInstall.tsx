import { useQuery } from '@tanstack/react-query';
import { Check, Copy, Download, Loader2, ShieldAlert, ShieldCheck } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { apiFetch, apiFetchBlob, formatError } from '@/lib/api';
import type { BackendSigningKey } from '@/lib/signing';
import { toast } from 'sonner';

const FALLBACK_FILENAME = 'kanade-agent-installer.zip';

// Pull the download filename out of the installer's Content-Disposition
// header (`attachment; filename="kanade-agent-installer-<version>.zip"` /
// `…-<version>-linux-<arch>.tar.gz`). Kept pure and exported so the parsing
// is testable without a DOM — the same reason lib/signing.ts stays out of
// its badge component.
export function installerFilename(contentDisposition: string | null): string {
  const m = /filename="?([^";]+)"?/i.exec(contentDisposition ?? '');
  return m?.[1]?.trim() || FALLBACK_FILENAME;
}

export type InstallerOs = 'windows' | 'linux';
type InstallerArch = 'x86_64' | 'aarch64';

// Initial OS for the toggle, guessed from the browser. `platform` is
// `navigator.userAgentData.platform` (Chromium) — more reliable than the UA
// string, which is frozen/reduced there — with the plain UA as fallback.
// 'Win' → windows, 'Linux'/'X11' → linux; anything else (including macOS,
// which has no installer) defaults to 'windows', the dominant endpoint OS.
// Pure + exported so the mapping is unit-testable without a DOM.
export function detectOs(ua: string, platform?: string): InstallerOs {
  const probe = platform || ua;
  if (probe.includes('Win')) return 'windows';
  if (probe.includes('Linux') || probe.includes('X11')) return 'linux';
  return 'windows';
}

// The copyable one-liner install command for each OS. The script endpoints
// (`installer.ps1` / `installer.sh`) are auth-gated like every other API
// route, so the command embeds the caller's session token as a Bearer
// header and points at the backend that served this SPA (`origin`) —
// correct even when the operator browses through a reverse proxy. Pure +
// exported so the shape is unit-testable.
export function oneLiner(os: InstallerOs, origin: string, token: string): string {
  if (os === 'linux') {
    // printf (a shell builtin — never a /proc cmdline entry) feeds curl a
    // config on stdin, so the token never appears in any process's argv:
    // /proc/<pid>/cmdline is world-readable on Linux. Two escaping layers:
    // `\`/`"` for the double-quoted curl-config value (unreachable with
    // today's JWT charset, pinned for future token formats), then `'` →
    // `'\''` for the single-quoted printf argument.
    const quoted = token.replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/'/g, `'\\''`);
    return `printf 'header = "Authorization: Bearer %s"\\n' '${quoted}' | curl -fsSL -K - ${origin}/api/agents/installer.sh | sudo bash`;
  }
  return `irm -Headers @{Authorization='Bearer ${token}'} ${origin}/api/agents/installer.ps1 | iex`;
}

// First-time agent install for END USERS (the download-only account): no
// options beyond the OS/arch — `GET /api/agents/installer` always builds
// the package from the latest release with the NATS settings and (on
// Windows, when the backend signs commands) the signing public key baked in
// server-side. The page is the only one a restricted installer account can
// reach; the endpoint is gated by the `agent-install` feature, not by role.
export function AgentInstall() {
  const { t } = useTranslation('agent-install');
  // Preselect the visitor's own OS — the common case is downloading the
  // installer on (or for) a machine of the same platform. userAgentData is
  // Chromium-only; absent elsewhere, the UA string carries the same hint.
  const [os, setOs] = useState<InstallerOs>(() =>
    detectOs(
      navigator.userAgent,
      (navigator as { userAgentData?: { platform?: string } }).userAgentData?.platform,
    ),
  );
  const [arch, setArch] = useState<InstallerArch>('x86_64');
  const [downloading, setDownloading] = useState(false);
  // Session token embedded into the one-liner — same accessor as
  // lib/api.ts / lib/auth.tsx (`localStorage.kanade_token`). Read once:
  // the embedded token expires with the session anyway, so live-tracking
  // changes buys nothing. Absent token (shouldn't happen behind the auth
  // gate) → the one-liner block is hidden.
  const [token] = useState(() => localStorage.getItem('kanade_token') ?? '');
  const [copied, setCopied] = useState(false);

  const command = token ? oneLiner(os, window.location.origin, token) : '';

  async function copyOneLiner() {
    // Same rule as MfaCard's copySecret: only claim success once the write
    // actually resolves — a false "copied" (insecure origin, denied
    // permission) would strand the operator.
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
      toast.success(t('oneLiner.copied'));
    } catch {
      toast.error(t('oneLiner.copyFailed'));
    }
  }

  // Same query key + staleTime as the Agents page: this backend's signing
  // key only changes on an operator rotation, which restarts the backend.
  // `/api/command-signing` is one of the few routes a restricted account may
  // still call, so this works for the download-only user too.
  const signingQ = useQuery({
    queryKey: ['command-signing'],
    queryFn: () => apiFetch<BackendSigningKey>('/api/command-signing'),
    staleTime: Infinity,
  });

  const signing = signingQ.data;
  const isSigning = !!signing?.kid && !!signing?.fingerprint;

  async function download() {
    setDownloading(true);
    try {
      let filename = FALLBACK_FILENAME;
      // Bare URL = Windows ZIP; Linux takes the platform query params
      // (arch defaults server-side to x86_64, sent explicitly anyway).
      const apiUrl =
        os === 'linux'
          ? `/api/agents/installer?os=linux&arch=${arch}`
          : '/api/agents/installer';
      const blob = await apiFetchBlob(apiUrl, {}, (res) => {
        filename = installerFilename(res.headers.get('Content-Disposition'));
      });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = filename;
      document.body.appendChild(a);
      a.click();
      a.remove();
      // Same deferred revoke as the Collect page — revoking immediately
      // after click() can abort the download in Safari before it starts.
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (e) {
      toast.error(formatError(e));
    } finally {
      setDownloading(false);
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <span className="text-xs text-muted">{t('intro')}</span>
      </div>

      {/* OS toggle — switches both the instructions and the download
          target. aria-pressed toggle buttons, not the WAI-ARIA tabs
          pattern: two options don't justify the roving-tabindex +
          arrow-key machinery the Settings tabs carry. */}
      <div className="inline-flex rounded-md border border-border bg-card text-sm overflow-hidden">
        {(['windows', 'linux'] as const).map((k) => (
          <button
            key={k}
            type="button"
            aria-pressed={os === k}
            onClick={() => setOs(k)}
            className={os === k ? 'px-4 h-9 bg-accent/15 text-accent' : 'px-4 h-9 hover:bg-accent/5'}
          >
            {t(`os.${k}`)}
          </button>
        ))}
      </div>

      <Card>
        <CardHeader>
          <CardTitle>{t('instructions.title')}</CardTitle>
          <CardDescription>{t('instructions.description')}</CardDescription>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          {os === 'windows' ? (
            <>
              <ol className="list-decimal space-y-1.5 pl-5">
                <li>{t('instructions.steps.one')}</li>
                <li>{t('instructions.steps.two')}</li>
                <li>
                  <Trans
                    ns="agent-install"
                    i18nKey="instructions.steps.three"
                    components={{ code: <code />, strong: <strong /> }}
                  />
                </li>
              </ol>
              <ul className="list-disc space-y-1 pl-5 text-muted">
                <li>{t('instructions.autoNote')}</li>
                <li>{t('instructions.windowsNote')}</li>
              </ul>
            </>
          ) : (
            <>
              <ol className="list-decimal space-y-1.5 pl-5">
                <li>{t('instructions.linux.steps.one')}</li>
                <li>
                  <Trans
                    ns="agent-install"
                    i18nKey="instructions.linux.steps.two"
                    components={{ code: <code /> }}
                  />
                </li>
                <li>
                  <Trans
                    ns="agent-install"
                    i18nKey="instructions.linux.steps.three"
                    components={{ code: <code /> }}
                  />
                </li>
              </ol>
              <ul className="list-disc space-y-1 pl-5 text-muted">
                <li>{t('instructions.linux.systemdNote')}</li>
                <li>{t('instructions.linux.signingNote')}</li>
              </ul>
            </>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('download.title')}</CardTitle>
          <CardDescription>
            <Trans
              ns="agent-install"
              i18nKey={os === 'linux' ? 'download.descriptionLinux' : 'download.description'}
              components={{ code: <code /> }}
            />
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-3">
          <div className="flex items-end gap-3">
            {os === 'linux' && (
              <div className="space-y-1">
                <Label htmlFor="ai-arch">{t('download.archLabel')}</Label>
                <Select
                  id="ai-arch"
                  value={arch}
                  onChange={(e) => setArch(e.target.value as InstallerArch)}
                  className="w-56"
                >
                  <option value="x86_64">{t('download.archOptions.x86_64')}</option>
                  <option value="aarch64">{t('download.archOptions.aarch64')}</option>
                </Select>
              </div>
            )}
            <Button onClick={download} disabled={downloading}>
              {downloading ? (
                <Loader2 className="size-4 mr-2 animate-spin" />
              ) : (
                <Download className="size-4 mr-2" />
              )}
              {t('download.downloadButton')}
            </Button>
          </div>
          {/* One-liner install — the same installer, fetched and run by a
              single pasted command on the target machine. The command
              embeds the session token, so it's hidden when there is none
              (shouldn't happen behind the auth gate). */}
          {token && (
            <div className="space-y-1.5">
              <p className="text-sm font-medium">{t(`oneLiner.label.${os}`)}</p>
              <div className="flex items-center gap-2">
                <code className="flex-1 overflow-x-auto whitespace-nowrap rounded-md border border-border bg-bg px-3 py-2 text-xs">
                  {command}
                </code>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={copyOneLiner}
                  title={t('oneLiner.copyTitle')}
                >
                  {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
                </Button>
              </div>
              <p className="text-xs text-muted">{t(`oneLiner.hint.${os}`)}</p>
              <p className="text-xs text-amber">{t('oneLiner.tokenWarning')}</p>
            </div>
          )}
          {/* #1260 status: whether this backend signs commands decides
              whether the package embeds a command-signing public key
              (Windows — Linux provisioning doesn't exist yet). About the
              backend, not the selected OS, so it shows on both tabs. */}
          {signingQ.error ? (
            <ErrorCard title={t('signing.errorTitle')} error={signingQ.error} />
          ) : signing && (
            <div className="flex items-start gap-2 text-sm">
              {isSigning ? (
                <>
                  <ShieldCheck className="size-4 mt-0.5 shrink-0 text-success" />
                  <span className="text-muted">
                    <Trans
                      ns="agent-install"
                      i18nKey="signing.enabled"
                      values={{ kid: signing.kid, fingerprint: signing.fingerprint }}
                      components={{ code: <code /> }}
                    />
                  </span>
                </>
              ) : (
                <>
                  <ShieldAlert className="size-4 mt-0.5 shrink-0 text-amber" />
                  <span className="text-muted">{t('signing.disabled')}</span>
                </>
              )}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
