import { useQuery } from '@tanstack/react-query';
import { Download, Loader2, ShieldAlert, ShieldCheck } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { apiFetch, apiFetchBlob, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import type { BackendSigningKey } from '@/lib/signing';
import { toast } from 'sonner';

// Mirror of the backend release row (also used by the Rollout page):
// `GET /api/agents/releases`, sorted newest-first.
type ReleaseRow = {
  version: string;
  size: number;
  digest: string | null;
  modified: string | null;
};

const FALLBACK_FILENAME = 'kanade-agent-installer.zip';

// Pull the download filename out of the installer's Content-Disposition
// header (`attachment; filename="kanade-agent-installer-<version>.zip"`).
// Kept pure and exported so the parsing is testable without a DOM —
// the same reason lib/signing.ts stays out of its badge component.
export function installerFilename(contentDisposition: string | null): string {
  const m = /filename="?([^";]+)"?/i.exec(contentDisposition ?? '');
  return m?.[1]?.trim() || FALLBACK_FILENAME;
}

export function AgentInstall() {
  const { t } = useTranslation('agent-install');
  const { hasRole } = useAuth();
  // POST /api/agents/installer is operator-gated server-side — mirror the
  // other pages' self-gating so a viewer sees why the button is disabled
  // instead of getting a 403 on click.
  const canOperate = hasRole('operator');
  // '' means "latest" — resolved to the first release row once loaded.
  const [version, setVersion] = useState('');
  const [natsUrl, setNatsUrl] = useState('');
  const [natsToken, setNatsToken] = useState('');
  const [downloading, setDownloading] = useState(false);

  const releasesQ = useQuery({
    queryKey: ['agent-releases'],
    queryFn: () => apiFetch<ReleaseRow[]>('/api/agents/releases'),
  });
  // Same query key + staleTime as the Agents page: this backend's signing
  // key only changes on an operator rotation, which restarts the backend.
  const signingQ = useQuery({
    queryKey: ['command-signing'],
    queryFn: () => apiFetch<BackendSigningKey>('/api/command-signing'),
    staleTime: Infinity,
  });

  const releases = releasesQ.data ?? [];
  const selectedVersion = version || releases[0]?.version || '';
  const signing = signingQ.data;
  const isSigning = !!signing?.kid && !!signing?.fingerprint;

  async function download() {
    setDownloading(true);
    try {
      let filename = FALLBACK_FILENAME;
      const blob = await apiFetchBlob(
        '/api/agents/installer',
        {
          method: 'POST',
          body: JSON.stringify({
            // Every field is optional server-side — omit empties so the
            // backend applies its own defaults (its configured NATS URL,
            // the latest release, …) instead of receiving "".
            ...(selectedVersion ? { version: selectedVersion } : {}),
            ...(natsUrl.trim() ? { nats_url: natsUrl.trim() } : {}),
            ...(natsToken ? { nats_token: natsToken } : {}),
          }),
        },
        (res) => {
          filename = installerFilename(res.headers.get('Content-Disposition'));
        },
      );
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

      <Card>
        <CardHeader>
          <CardTitle>{t('instructions.title')}</CardTitle>
          <CardDescription>{t('instructions.description')}</CardDescription>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
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
            <li>{t('instructions.breakGlassNote')}</li>
          </ul>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t('download.title')}</CardTitle>
          <CardDescription>
            <Trans ns="agent-install" i18nKey="download.description" components={{ code: <code /> }} />
          </CardDescription>
        </CardHeader>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          <div className="space-y-1">
            <Label htmlFor="ai-version">{t('download.versionLabel')}</Label>
            <Select
              id="ai-version"
              value={selectedVersion}
              onChange={(e) => setVersion(e.target.value)}
              disabled={releasesQ.isLoading || releases.length === 0}
            >
              {releases.map((r) => (
                <option key={r.version} value={r.version}>
                  {r.version}
                </option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ai-nats-url">{t('download.natsUrlLabel')}</Label>
            <Input
              id="ai-nats-url"
              placeholder={t('download.natsUrlPlaceholder')}
              value={natsUrl}
              onChange={(e) => setNatsUrl(e.target.value)}
            />
            <p className="text-xs text-muted">
              {t('download.natsUrlHint')}
            </p>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ai-nats-token">{t('download.natsTokenLabel')}</Label>
            <Input
              id="ai-nats-token"
              type="password"
              placeholder={t('download.natsTokenPlaceholder')}
              value={natsToken}
              onChange={(e) => setNatsToken(e.target.value)}
            />
          </div>
        </CardContent>
        <CardContent className="flex flex-col gap-3 pt-0">
          {releasesQ.error ? (
            <ErrorCard title={t('download.errorTitle')} error={releasesQ.error} />
          ) : !releasesQ.isLoading && releases.length === 0 ? (
            <div className="text-muted text-sm">
              <Trans ns="agent-install" i18nKey="download.noReleases" components={{ code: <code /> }} />
            </div>
          ) : null}
          <div className="flex items-center gap-3">
            <Button
              onClick={download}
              disabled={
                !canOperate || downloading || releasesQ.isLoading || !!releasesQ.error || releases.length === 0
              }
              title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
            >
              {downloading ? (
                <Loader2 className="size-4 mr-2 animate-spin" />
              ) : (
                <Download className="size-4 mr-2" />
              )}
              {t('download.downloadButton')}
            </Button>
          </div>
          {!canOperate && (
            <p className="text-xs text-muted">{t('rbac.operatorRequired', { ns: 'common' })}</p>
          )}
          {/* #1260 status: whether this backend signs commands decides
              whether the ZIP can embed a command-signing public key. */}
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
