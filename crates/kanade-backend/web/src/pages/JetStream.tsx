import { useQuery } from '@tanstack/react-query';
import { CheckCircle2, Loader2, XCircle } from 'lucide-react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { JetstreamSnapshot } from '@/lib/types';

function ProbeTable({ rows }: { rows: { name: string; exists: boolean }[] }) {
  const { t } = useTranslation('jetstream');
  return (
    <Table>
      <TableHeader>
        <TableRow><TableHead>{t('columns.name')}</TableHead><TableHead>{t('columns.status')}</TableHead></TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((r) => (
          <TableRow key={r.name}>
            <TableCell label={t('columns.name')}><code className="text-xs">{r.name}</code></TableCell>
            <TableCell label={t('columns.status')}>
              {r.exists
                ? <span className="inline-flex items-center gap-1 text-success"><CheckCircle2 className="size-4" />{t('status.ok')}</span>
                : <span className="inline-flex items-center gap-1 text-danger"><XCircle className="size-4" />{t('status.missing')}</span>}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

export function JetStream() {
  const { t } = useTranslation('jetstream');
  const { data, error, isLoading } = useQuery({
    queryKey: ['jetstream-status'],
    queryFn: () => apiFetch<JetstreamSnapshot>('/api/jetstream/status'),
  });

  if (isLoading) return <div className="flex items-center gap-2 text-muted"><Loader2 className="size-4 animate-spin" />{t('loading')}</div>;
  if (error) return <ErrorCard title={t('errorTitle')} error={error} />;
  if (!data) return null;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>{t('title')}</CardTitle>
          <CardDescription>
            <Trans ns="jetstream" i18nKey="description" components={{ code: <code /> }} />
          </CardDescription>
        </CardHeader>
      </Card>
      <section className="space-y-2">
        <h3 className="text-base font-bold">{t('sections.streams')}</h3>
        <ProbeTable rows={data.streams} />
      </section>
      <section className="space-y-2">
        <h3 className="text-base font-bold">{t('sections.kvBuckets')}</h3>
        <ProbeTable rows={data.kv_buckets} />
      </section>
      <section className="space-y-2">
        <h3 className="text-base font-bold">{t('sections.objectStores')}</h3>
        <ProbeTable rows={data.object_stores} />
      </section>
    </div>
  );
}
