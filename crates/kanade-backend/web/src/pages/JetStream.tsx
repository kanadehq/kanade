import { useQuery } from '@tanstack/react-query';
import { CheckCircle2, Loader2, XCircle } from 'lucide-react';
import { Trans, useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { JetstreamProbe, JetstreamSnapshot } from '@/lib/types';

/** Human-readable IEC bytes (1024-based, matching JetStream's own units). */
function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** Usage bar for one resource. Capped resources show used/cap + a fill
 *  colored by fullness; uncapped (curated) stores show the raw size with a
 *  neutral "no cap" track; an unreadable probe shows a dash. */
function UsageBar({ probe }: { probe: JetstreamProbe }) {
  const { t } = useTranslation('jetstream');
  if (probe.bytes == null) {
    return <span className="text-muted">—</span>;
  }
  const used = probe.bytes;

  if (probe.max_bytes == null) {
    // Uncapped, operator-curated store (agent_releases / app_packages /
    // scripts). No percentage to show — just the size.
    return (
      <div className="min-w-40">
        <div className="h-2 w-full rounded-full bg-muted/20" />
        <div className="mt-1 text-xs text-muted">
          {fmtBytes(used)} · {t('usage.noCap')}
        </div>
      </div>
    );
  }

  const cap = probe.max_bytes;
  const pct = cap > 0 ? Math.min(100, (used / cap) * 100) : 0;
  // Green under 70%, amber to 90%, red past 90% — the point where
  // discard:Old starts trimming useful history soon.
  const fill = pct >= 90 ? 'bg-danger' : pct >= 70 ? 'bg-amber' : 'bg-success';

  return (
    <div className="min-w-40">
      <div
        className="h-2 w-full overflow-hidden rounded-full bg-muted/20"
        role="progressbar"
        aria-valuenow={Math.round(pct)}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={t('usage.aria', { name: probe.name })}
      >
        <div className={`h-full ${fill}`} style={{ width: `${pct}%` }} />
      </div>
      <div className="mt-1 text-xs text-muted">
        {fmtBytes(used)} / {fmtBytes(cap)} · {pct.toFixed(pct < 10 ? 1 : 0)}%
      </div>
    </div>
  );
}

function ProbeTable({ rows }: { rows: JetstreamProbe[] }) {
  const { t } = useTranslation('jetstream');
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>{t('columns.name')}</TableHead>
          <TableHead>{t('columns.status')}</TableHead>
          <TableHead>{t('columns.usage')}</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((r) => (
          <TableRow key={r.name}>
            <TableCell label={t('columns.name')}>
              <code className="text-xs">{r.name}</code>
            </TableCell>
            <TableCell label={t('columns.status')}>
              {r.exists ? (
                <span className="inline-flex items-center gap-1 text-success">
                  <CheckCircle2 className="size-4" />
                  {t('status.ok')}
                </span>
              ) : (
                <span className="inline-flex items-center gap-1 text-danger">
                  <XCircle className="size-4" />
                  {t('status.missing')}
                </span>
              )}
            </TableCell>
            <TableCell label={t('columns.usage')}>
              <UsageBar probe={r} />
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

  if (isLoading)
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />
        {t('loading')}
      </div>
    );
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
