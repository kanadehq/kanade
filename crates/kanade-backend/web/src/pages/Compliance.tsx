// Compliance page (#290 PR-E2). Renders the fleet-wide `check_status`
// rows from GET /api/checks — which PCs pass / warn / fail / unknown
// for each operator-defined `check:` job — grouped by check. This is
// the operator-facing counterpart to the end-user Client App's Health
// tab: a check written with just `check:` shows up here for free
// (unless the operator set `fleet: false`).

import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo, useState } from 'react';
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

// #497: the API now returns attention rows (status != ok) plus
// complete per-check counts; the ok bulk is fetched per check on
// demand. At fleet scale the old all-rows shape was 3,000 × K rows
// per 60 s poll for a healthy fleet.
type CheckCounts = {
  check_name: string;
  ok: number;
  warn: number;
  fail: number;
  unknown: number;
};

type ChecksResponse = {
  counts: CheckCounts[];
  rows: CheckRow[];
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

// #497: per-card ok-rows expansion. Mounted only after the operator
// asks for the healthy bulk of one check; the response is the full
// check (attention + ok) so the table swaps wholesale.
function OkRows({ check }: { check: string }) {
  const q = useQuery({
    queryKey: ['checks', check, 'full'],
    queryFn: () =>
      apiFetch<ChecksResponse>(
        `/api/checks?check=${encodeURIComponent(check)}&include_ok=true`,
      ),
    staleTime: 60_000,
  });
  const { t } = useTranslation('compliance');
  if (q.isLoading) {
    return (
      <TableRow>
        <TableCell colSpan={4} className="text-muted text-sm">
          <Loader2 className="size-3.5 animate-spin inline mr-1" />
          {t('loading')}
        </TableCell>
      </TableRow>
    );
  }
  // Attention rows already render above; append just the ok rows so
  // the list doesn't duplicate.
  const okOnly = (q.data?.rows ?? []).filter((r) => r.status === 'ok');
  return (
    <>
      {okOnly.map((r) => (
        <CheckTableRow key={`${r.pc_id}/${r.check_name}`} row={r} />
      ))}
    </>
  );
}

export function Compliance() {
  const { t } = useTranslation('compliance');
  const q = useQuery({
    queryKey: ['checks'],
    queryFn: () => apiFetch<ChecksResponse>('/api/checks'),
    // Mirror the other live-data views (Inventory: 60s) so a fixed
    // host doesn't keep showing failing until the operator refocuses.
    refetchInterval: 60_000,
  });
  // #497: which checks the operator expanded to include the ok bulk.
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const groups = useMemo<CheckGroup[]>(() => {
    // Groups derive from COUNTS (complete), so an all-ok check still
    // gets its card; rows carry only the attention subset.
    const byCheck = new Map<string, CheckRow[]>();
    for (const r of q.data?.rows ?? []) {
      const list = byCheck.get(r.check_name) ?? [];
      list.push(r);
      byCheck.set(r.check_name, list);
    }
    return (q.data?.counts ?? [])
      .slice()
      .sort((a, b) => a.check_name.localeCompare(b.check_name))
      .map((c) => ({
        name: c.check_name,
        rows: (byCheck.get(c.check_name) ?? [])
          .slice()
          .sort((a, b) => a.pc_id.localeCompare(b.pc_id)),
        counts: { ok: c.ok, warn: c.warn, fail: c.fail, unknown: c.unknown },
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
              <div className="flex flex-wrap items-center gap-1">
                {STATUS_ORDER.filter((s) => g.counts[s] > 0).map((s) => (
                  <Badge key={s} variant={STATUS_VARIANT[s]}>
                    {t(`status.${s}`)} {g.counts[s]}
                  </Badge>
                ))}
                {g.counts.ok > 0 && (
                  <button
                    type="button"
                    className="text-xs text-muted underline ml-2"
                    onClick={() =>
                      setExpanded((prev) => {
                        const next = new Set(prev);
                        if (next.has(g.name)) next.delete(g.name);
                        else next.add(g.name);
                        return next;
                      })
                    }
                  >
                    {expanded.has(g.name)
                      ? t('hideOk')
                      : t('showOk', { count: g.counts.ok })}
                  </button>
                )}
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
                    <CheckTableRow key={`${r.pc_id}/${r.check_name}`} row={r} />
                  ))}
                  {g.rows.length === 0 && !expanded.has(g.name) && (
                    <TableRow>
                      <TableCell colSpan={4} className="text-muted text-sm text-center py-4">
                        {t('allOk', { count: g.counts.ok })}
                      </TableCell>
                    </TableRow>
                  )}
                  {expanded.has(g.name) && <OkRows check={g.name} />}
                </TableBody>
              </Table>
            </CardContent>
          </Card>
        ))
      )}
    </div>
  );
}

function CheckTableRow({ row: r }: { row: CheckRow }) {
  const { t } = useTranslation('compliance');
  return (
    <TableRow>
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
  );
}
