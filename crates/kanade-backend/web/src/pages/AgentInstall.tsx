import { useQuery } from '@tanstack/react-query';
import { Download, Loader2, ShieldAlert, ShieldCheck } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { apiFetch, apiFetchBlob, formatError } from '@/lib/api';
import type { BackendSigningKey } from '@/lib/signing';
import { toast } from 'sonner';

const FALLBACK_FILENAME = 'kanade-agent-installer.zip';

// Pull the download filename out of the installer's Content-Disposition
// header (`attachment; filename="kanade-agent-installer-<version>.zip"`).
// Kept pure and exported so the parsing is testable without a DOM —
// the same reason lib/signing.ts stays out of its badge component.
export function installerFilename(contentDisposition: string | null): string {
  const m = /filename="?([^";]+)"?/i.exec(contentDisposition ?? '');
  return m?.[1]?.trim() || FALLBACK_FILENAME;
}

// First-time agent install for END USERS (the download-only account): no
// options, no version picker — `GET /api/agents/installer` always builds the
// ZIP from the latest release with the NATS settings and (when the backend
// signs commands) the signing public key baked in server-side. The page is
// the only one a restricted installer account can reach; the endpoint is
// gated by the `agent-install` feature, not by role.
export function AgentInstall() {
  const { t } = useTranslation('agent-install');
  const [downloading, setDownloading] = useState(false);

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
      const blob = await apiFetchBlob('/api/agents/installer', {}, (res) => {
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
        <CardContent className="flex flex-col gap-3">
          <div className="flex items-center gap-3">
            <Button onClick={download} disabled={downloading}>
              {downloading ? (
                <Loader2 className="size-4 mr-2 animate-spin" />
              ) : (
                <Download className="size-4 mr-2" />
              )}
              {t('download.downloadButton')}
            </Button>
          </div>
          {/* #1260 status: whether this backend signs commands decides
              whether the ZIP embeds a command-signing public key. */}
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
