import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Download, Loader2, Trash2 } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, apiFetchBlob, formatError } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';
import { toast } from 'sonner';

// Mirror of the backend `BundleRow` (api/collect.rs). `name` /
// `description` come from the producing job's `collect:` hint when the
// manifest still exists; `collected_at` is the key's timestamp segment.
type BundleRow = {
  key: string;
  pc_id: string;
  job_id: string;
  collected_at: string | null;
  // Present when a run produced multiple bundles (e.g. a per-day label
  // like "20260101"); null for the single-bundle form.
  label: string | null;
  size: number;
  digest: string | null;
  name: string | null;
  description: string | null;
};

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}

// The bundle key carries slashes (`<pc_id>/<job_id>/<ts>.zip`) that are
// real path segments of the `{*key}` wildcard route — keep them, but
// percent-encode each segment so an unusual pc_id can't break the URL.
function bundleUrl(key: string): string {
  return `/api/collect/bundles/${key.split('/').map(encodeURIComponent).join('/')}`;
}

export function Collect() {
  const { t } = useTranslation('collect');
  const qc = useQueryClient();
  const confirm = useConfirm();
  const [downloading, setDownloading] = useState<string | null>(null);

  const listQ = useQuery({
    queryKey: ['collect-bundles'],
    queryFn: () => apiFetch<BundleRow[]>('/api/collect/bundles'),
  });

  const remove = useMutation({
    mutationFn: async (key: string) => {
      await apiFetch<void>(bundleUrl(key), { method: 'DELETE' });
      return key;
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ['collect-bundles'] }),
  });

  async function download(row: BundleRow) {
    setDownloading(row.key);
    try {
      const blob = await apiFetchBlob(bundleUrl(row.key));
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      // Flat, filesystem-safe name from the key.
      a.download = row.key.replace(/\//g, '-');
      document.body.appendChild(a);
      a.click();
      a.remove();
      // Defer revocation — revoking immediately after click() can abort
      // the download in some browsers (Safari) before it starts.
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (e) {
      toast.error(formatError(e));
    } finally {
      setDownloading(null);
    }
  }

  const rows = listQ.data ?? [];

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <span className="text-xs text-muted">
          {/* No trigger UI here — collections are fired from the Exec
              page (or the Client App for `collect:`+`client:` jobs). */}
          <Trans ns="collect" i18nKey="intro" components={{ code: <code /> }} />
        </span>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>{t('bundles.title')}</CardTitle>
          <CardDescription>
            <Trans ns="collect" i18nKey="bundles.description" components={{ code: <code /> }} />
          </CardDescription>
        </CardHeader>
        <CardContent>
          {listQ.isLoading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="size-4 animate-spin" />
              {t('bundles.loading')}
            </div>
          ) : listQ.error ? (
            <ErrorCard title={t('bundles.errorTitle')} error={listQ.error} />
          ) : rows.length === 0 ? (
            <div className="text-muted text-sm">{t('bundles.empty')}</div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('bundles.columns.name')}</TableHead>
                  <TableHead>{t('bundles.columns.pc')}</TableHead>
                  <TableHead>{t('bundles.columns.job')}</TableHead>
                  <TableHead>{t('bundles.columns.collectedAt')}</TableHead>
                  <TableHead>{t('bundles.columns.size')}</TableHead>
                  <TableHead className="text-right">{t('bundles.columns.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((r) => (
                  <TableRow key={r.key}>
                    <TableCell>
                      <div className="text-sm">
                        {r.name ?? r.job_id}
                        {r.label && (
                          <span className="ml-2 rounded bg-muted/40 px-1.5 py-0.5 text-xs text-muted">
                            {r.label}
                          </span>
                        )}
                      </div>
                      {r.description && (
                        <div className="text-xs text-muted">{r.description}</div>
                      )}
                    </TableCell>
                    <TableCell><code className="text-xs">{r.pc_id}</code></TableCell>
                    <TableCell><code className="text-xs break-all">{r.job_id}</code></TableCell>
                    <TableCell className="text-muted text-xs">{fmtIsoLocal(r.collected_at)}</TableCell>
                    <TableCell className="text-muted text-xs">{fmtSize(r.size)}</TableCell>
                    <TableCell className="text-right">
                      <div className="inline-flex gap-2">
                        <Button
                          size="sm"
                          variant="secondary"
                          onClick={() => download(r)}
                          // Disable ALL download buttons while any one is
                          // active: `downloading` is a single key, so
                          // concurrent downloads would race the spinner +
                          // pile up large blobs in memory.
                          disabled={!!downloading}
                          title={t('bundles.downloadTitle')}
                        >
                          {downloading === r.key ? (
                            <Loader2 className="size-3.5 animate-spin" />
                          ) : (
                            <Download className="size-3.5" />
                          )}
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          onClick={async () => {
                            const ok = await confirm({
                              title: t('bundles.confirmDeleteTitle', { key: r.key }),
                              description: t('bundles.confirmDeleteDescription'),
                              confirmLabel: t('bundles.confirmDeleteButton'),
                              danger: true,
                            });
                            if (ok) remove.mutate(r.key);
                          }}
                          disabled={remove.isPending}
                          title={t('bundles.deleteTitle')}
                        >
                          {remove.isPending && remove.variables === r.key ? (
                            <Loader2 className="size-3.5 animate-spin" />
                          ) : (
                            <Trash2 className="size-3.5" />
                          )}
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
          {remove.error && (
            <div className="mt-3">
              <ErrorCard title={t('bundles.deleteErrorTitle')} error={remove.error} />
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
