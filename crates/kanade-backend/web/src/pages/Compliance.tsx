// Compliance page (#290 PR-E2). Renders the fleet-wide `check_status`
// rows from GET /api/checks — which PCs pass / warn / fail / unknown
// for each operator-defined `check:` job — grouped by check. This is
// the operator-facing counterpart to the end-user Client App's Health
// tab: a check written with just `check:` shows up here for free
// (unless the operator set `fleet: false`).

import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

type CheckRow = {
  pc_id: string;
  check_name: string;
  status: 'ok' | 'warn' | 'fail' | 'unknown' | string;
  detail: string | null;
  recorded_at: string;
};

type BadgeVariant = 'success' | 'amber' | 'danger' | 'default';

const STATUS_VARIANT: Record<string, BadgeVariant> = {
  ok: 'success',
  warn: 'amber',
  fail: 'danger',
  unknown: 'default',
};

// Worst-first so a card's badges lead with what needs attention.
const STATUS_ORDER = ['fail', 'warn', 'unknown', 'ok'] as const;

type CheckGroup = {
  name: string;
  rows: CheckRow[];
  counts: Record<string, number>;
};

export function Compliance() {
  const { t } = useTranslation('compliance');
  const q = useQuery({
    queryKey: ['checks'],
    queryFn: () => apiFetch<CheckRow[]>('/api/checks'),
    // Mirror the other live-data views (Inventory: 60s) so a fixed
    // host doesn't keep showing failing until the operator refocuses.
    refetchInterval: 60_000,
  });

  const groups = useMemo<CheckGroup[]>(() => {
    const byCheck = new Map<string, CheckRow[]>();
    for (const r of q.data ?? []) {
      const list = byCheck.get(r.check_name) ?? [];
      list.push(r);
      byCheck.set(r.check_name, list);
    }
    return [...byCheck.entries()]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, rows]) => ({
        name,
        rows: [...rows].sort((a, b) => a.pc_id.localeCompare(b.pc_id)),
        counts: countByStatus(rows),
      }));
  }, [q.data]);

  if (q.isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" /> {t('loading')}
      </div>
    );
  }
  if (q.isError) {
    return <ErrorCard title={t('errorTitle')} error={q.error} />;
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <Badge variant="violet">{t('checkBadge', { count: groups.length })}</Badge>
      </div>
      <p className="text-sm text-muted">{t('description')}</p>

      {groups.length === 0 ? (
        <Card>
          <CardContent className="py-8 text-center text-muted">{t('empty')}</CardContent>
        </Card>
      ) : (
        groups.map((g) => (
          <Card key={g.name}>
            <CardHeader className="flex flex-row items-center justify-between gap-2">
              <CardTitle className="text-base">{g.name}</CardTitle>
              <div className="flex flex-wrap gap-1">
                {STATUS_ORDER.filter((s) => g.counts[s] > 0).map((s) => (
                  <Badge key={s} variant={STATUS_VARIANT[s]}>
                    {t(`status.${s}`)} {g.counts[s]}
                  </Badge>
                ))}
              </div>
            </CardHeader>
            <CardContent>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t('col.pc')}</TableHead>
                    <TableHead>{t('col.status')}</TableHead>
                    <TableHead>{t('col.detail')}</TableHead>
                    <TableHead>{t('col.updated')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {g.rows.map((r) => (
                    <TableRow key={`${r.pc_id}/${r.check_name}`}>
                      <TableCell className="font-medium">{r.pc_id}</TableCell>
                      <TableCell>
                        <Badge variant={STATUS_VARIANT[r.status] ?? 'default'}>
                          {t(`status.${r.status}`, { defaultValue: r.status })}
                        </Badge>
                      </TableCell>
                      <TableCell className="text-muted">{r.detail ?? '—'}</TableCell>
                      <TableCell className="whitespace-nowrap text-muted">
                        {fmtIsoLocal(r.recorded_at)}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </CardContent>
          </Card>
        ))
      )}
    </div>
  );
}

function countByStatus(rows: CheckRow[]): Record<string, number> {
  const counts: Record<string, number> = { ok: 0, warn: 0, fail: 0, unknown: 0 };
  for (const r of rows) {
    counts[r.status] = (counts[r.status] ?? 0) + 1;
  }
  return counts;
}
