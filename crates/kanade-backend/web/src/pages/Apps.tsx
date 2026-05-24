import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { CheckCircle2, FileCode, Loader2, Package, Trash2, Upload } from 'lucide-react';
import { useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

// Both backend endpoints return the same row shape — the bucket
// split is purely operational (lifecycle + audit channel; see
// kanade-shared::kv).
type StoreRow = {
  name: string;
  version: string;
  size: number;
  digest: string | null;
  modified: string | null;
};

type SectionConfig = {
  /// i18n sub-namespace under `apps`. Keys live at
  /// `apps.{ns}.{key}` so the same `apps.json` carries both
  /// sections' strings without colliding.
  ns: 'appPackages' | 'scriptObjects';
  /// React Query cache key.
  queryKey: readonly string[];
  /// Backend list / CRUD root. POST / DELETE append `/{name}/{version}`.
  endpoint: string;
  icon: React.ReactNode;
  /// `<input accept="…">` hint. Loose by design — operators
  /// upload all kinds of installers / scripts / archives.
  accept?: string;
};

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

/// Generic Object Store section — list rows + upload form +
/// per-row delete. Both `OBJECT_APP_PACKAGES` (#207) and
/// `OBJECT_SCRIPTS` (#211) have identical HTTP shape, so factor
/// out the React glue. Each section translates against a
/// `SectionConfig.ns` sub-namespace.
function ObjectStoreSection({ ns, queryKey, endpoint, icon, accept }: SectionConfig) {
  const { t } = useTranslation('apps');
  const qc = useQueryClient();
  const confirm = useConfirm();
  const [uploadFile, setUploadFile] = useState<File | null>(null);
  const [name, setName] = useState('');
  const [version, setVersion] = useState('');

  const listQ = useQuery({
    queryKey: [...queryKey],
    queryFn: () => apiFetch<StoreRow[]>(endpoint),
  });

  type PublishResponse = {
    name: string;
    version: string;
    size: number;
    digest: string | null;
  };

  const upload = useMutation({
    mutationFn: async () => {
      if (!uploadFile) throw new Error(t(`${ns}.upload.noFileError`));
      if (!name.trim()) throw new Error(t(`${ns}.upload.noNameError`));
      if (!version.trim()) throw new Error(t(`${ns}.upload.noVersionError`));
      const fd = new FormData();
      fd.append('file', uploadFile);
      // apiFetch now skips its default JSON Content-Type for
      // FormData bodies (Gemini #218 HIGH), so the browser can
      // fill in `multipart/form-data; boundary=…` itself. Auth +
      // X-Kanade-Source still come from the central wrapper.
      return apiFetch<PublishResponse>(
        `${endpoint}/${encodeURIComponent(name.trim())}/${encodeURIComponent(version.trim())}`,
        { method: 'POST', body: fd },
      );
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [...queryKey] });
      setUploadFile(null);
      // Keep name/version so a re-upload of a corrected build
      // doesn't force the operator to retype them.
    },
  });

  const remove = useMutation({
    mutationFn: async (target: { name: string; version: string }) => {
      await apiFetch<void>(
        `${endpoint}/${encodeURIComponent(target.name)}/${encodeURIComponent(target.version)}`,
        { method: 'DELETE' },
      );
      return target;
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: [...queryKey] });
    },
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          {icon}
          {t(`${ns}.title`)}
        </CardTitle>
        <CardDescription>
          <Trans ns="apps" i18nKey={`${ns}.description`} components={{ code: <code /> }} />
        </CardDescription>
      </CardHeader>

      <CardContent className="grid grid-cols-1 sm:grid-cols-3 gap-3">
        <div className="space-y-1">
          <Label htmlFor={`${ns}-name`}>{t(`${ns}.upload.nameLabel`)}</Label>
          <Input
            id={`${ns}-name`}
            placeholder={t(`${ns}.upload.namePlaceholder`)}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={`${ns}-version`}>{t(`${ns}.upload.versionLabel`)}</Label>
          <Input
            id={`${ns}-version`}
            placeholder={t(`${ns}.upload.versionPlaceholder`)}
            value={version}
            onChange={(e) => setVersion(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={`${ns}-file`}>{t(`${ns}.upload.fileLabel`)}</Label>
          <Input
            id={`${ns}-file`}
            type="file"
            accept={accept}
            onChange={(e) => setUploadFile(e.target.files?.[0] ?? null)}
          />
        </div>
      </CardContent>

      <CardContent className="flex flex-wrap items-center gap-3 pt-0">
        <Button
          onClick={() => upload.mutate()}
          disabled={
            !uploadFile || !name.trim() || !version.trim() || upload.isPending
          }
        >
          {upload.isPending ? (
            <Loader2 className="size-4 mr-2 animate-spin" />
          ) : (
            <Upload className="size-4 mr-2" />
          )}
          {t(`${ns}.upload.uploadButton`)}
        </Button>
        {uploadFile && (
          <span className="text-xs text-muted">
            {uploadFile.name} · {fmtSize(uploadFile.size)}
          </span>
        )}
        {upload.isSuccess && upload.data && (
          <span className="flex items-center gap-2 text-sm text-success">
            <CheckCircle2 className="size-4" />
            {t(`${ns}.upload.successPrefix`)}
            <code className="text-xs">
              {upload.data.name}/{upload.data.version}
            </code>
            ({fmtSize(upload.data.size)})
          </span>
        )}
      </CardContent>

      {upload.error && (
        <CardContent className="pt-0">
          <ErrorCard title={t(`${ns}.upload.errorTitle`)} error={upload.error} />
        </CardContent>
      )}

      <CardContent>
        {listQ.isLoading ? (
          <div className="flex items-center gap-2 text-muted">
            <Loader2 className="size-4 animate-spin" />
            {t(`${ns}.list.loading`)}
          </div>
        ) : listQ.error ? (
          <ErrorCard title={t(`${ns}.list.errorTitle`)} error={listQ.error} />
        ) : (listQ.data ?? []).length === 0 ? (
          <div className="text-muted text-sm">
            <Trans ns="apps" i18nKey={`${ns}.list.empty`} components={{ code: <code /> }} />
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t(`${ns}.list.columns.name`)}</TableHead>
                <TableHead>{t(`${ns}.list.columns.version`)}</TableHead>
                <TableHead>{t(`${ns}.list.columns.size`)}</TableHead>
                <TableHead>{t(`${ns}.list.columns.modified`)}</TableHead>
                <TableHead>{t(`${ns}.list.columns.digest`)}</TableHead>
                <TableHead className="text-right">{t(`${ns}.list.columns.actions`)}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(listQ.data ?? []).map((r) => (
                <TableRow key={`${r.name}/${r.version}`}>
                  <TableCell><code className="text-xs">{r.name}</code></TableCell>
                  <TableCell><code className="text-xs">{r.version}</code></TableCell>
                  <TableCell className="text-muted text-xs">{fmtSize(r.size)}</TableCell>
                  <TableCell className="text-muted text-xs">{fmtIsoLocal(r.modified)}</TableCell>
                  <TableCell className="text-muted text-xs">
                    <code>{r.digest?.slice(0, 24) ?? '—'}</code>
                  </TableCell>
                  <TableCell className="text-right">
                    <Button
                      size="sm"
                      variant="danger"
                      onClick={async () => {
                        const ok = await confirm({
                          title: t(`${ns}.list.confirmDeleteTitle`, {
                            name: r.name,
                            version: r.version,
                          }),
                          description: t(`${ns}.list.confirmDeleteDescription`),
                          confirmLabel: t(`${ns}.list.confirmDeleteButton`),
                          danger: true,
                        });
                        if (ok) {
                          remove.mutate({ name: r.name, version: r.version });
                        }
                      }}
                      disabled={remove.isPending}
                    >
                      <Trash2 className="size-3.5" />
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
        {remove.error && (
          <div className="mt-3">
            <ErrorCard title={t(`${ns}.list.deleteErrorTitle`)} error={remove.error} />
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function Apps() {
  const { t } = useTranslation('apps');
  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <span className="text-xs text-muted">
          <Trans ns="apps" i18nKey="intro" components={{ code: <code /> }} />
        </span>
      </div>

      <ObjectStoreSection
        ns="appPackages"
        queryKey={['app-packages']}
        endpoint="/api/app-packages"
        icon={<Package className="size-5 text-violet" />}
        accept=".exe,.msi,.zip,application/octet-stream"
      />

      <ObjectStoreSection
        ns="scriptObjects"
        queryKey={['script-objects']}
        endpoint="/api/script-objects"
        icon={<FileCode className="size-5 text-teal" />}
        accept=".ps1,.sh,.bat,.cmd,text/plain"
      />
    </div>
  );
}
