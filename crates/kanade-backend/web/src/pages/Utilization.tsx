import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

type AgentRow = { pc_id: string };

type UtilizationResponse = {
  pc_id: string;
  from: string;
  to: string;
  active: {
    total_samples: number;
    active_samples: number;
    active_ratio: number;
    first_active: string | null;
    last_active: string | null;
    est_active_minutes: number;
  };
  top_apps: { app: string; samples: number }[];
  top_sites: { host: string; visits: number }[];
  site_visits_capped: boolean;
};

// Local calendar day → UTC [from, to) bounds, so the day boundary is the
// operator's, not UTC's. setDate(+1) (not +86_400_000 ms) so DST change
// days still land on the next calendar midnight.
function dayBounds(date: string): { from: string; to: string } {
  const start = new Date(`${date}T00:00:00`);
  const end = new Date(start);
  end.setDate(end.getDate() + 1);
  return { from: start.toISOString(), to: end.toISOString() };
}

function todayLocal(): string {
  const d = new Date();
  const off = d.getTimezoneOffset() * 60_000;
  return new Date(d.getTime() - off).toISOString().slice(0, 10);
}

function fmtMinutes(min: number): string {
  const h = Math.floor(min / 60);
  const m = min % 60;
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

export function Utilization() {
  const { t } = useTranslation('utilization');
  const [pc, setPc] = useState('');
  const [date, setDate] = useState(todayLocal());

  const agentsQ = useQuery({
    queryKey: ['agents-min'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
  });
  // Default to the first agent once the list loads.
  const pcId = pc || agentsQ.data?.[0]?.pc_id || '';

  const { from, to } = useMemo(() => dayBounds(date), [date]);

  const q = useQuery({
    queryKey: ['utilization', pcId, from, to],
    queryFn: () =>
      apiFetch<UtilizationResponse>(
        `/api/utilization/${encodeURIComponent(pcId)}?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`,
      ),
    enabled: !!pcId,
  });

  const a = q.data?.active;

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <h2 className="text-xl">{t('title')}</h2>
        <div className="flex flex-wrap items-end gap-3">
          <div className="space-y-1">
            <Label htmlFor="util-pc">{t('controls.pc')}</Label>
            <select
              id="util-pc"
              value={pcId}
              onChange={(e) => setPc(e.target.value)}
              className="h-9 rounded-md border border-border bg-card px-2 text-sm"
            >
              {(agentsQ.data ?? []).map((ag) => (
                <option key={ag.pc_id} value={ag.pc_id}>
                  {ag.pc_id}
                </option>
              ))}
            </select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="util-date">{t('controls.date')}</Label>
            <Input
              id="util-date"
              type="date"
              value={date}
              max={todayLocal()}
              onChange={(e) => setDate(e.target.value)}
              className="w-40"
            />
          </div>
        </div>
      </div>

      {agentsQ.isLoading || (pcId && q.isLoading) ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />
          {t('loading')}
        </div>
      ) : !pcId ? (
        <div className="text-muted text-sm">{t('noAgents')}</div>
      ) : q.error ? (
        <ErrorCard title={t('errorTitle')} error={q.error} />
      ) : (
        <>
          {/* Active summary */}
          <Card>
            <CardHeader>
              <CardTitle className="text-sm">{t('active.title')}</CardTitle>
            </CardHeader>
            <CardContent className="grid grid-cols-2 gap-4 sm:grid-cols-4">
              <Stat
                label={t('active.ratio')}
                value={a ? `${Math.round(a.active_ratio * 100)}%` : '—'}
              />
              <Stat
                label={t('active.estHours')}
                value={a ? fmtMinutes(a.est_active_minutes) : '—'}
                hint={a ? t('active.estHint', { active: a.active_samples, total: a.total_samples }) : undefined}
              />
              <Stat label={t('active.first')} value={a?.first_active ? fmtIsoLocal(a.first_active) : '—'} />
              <Stat label={t('active.last')} value={a?.last_active ? fmtIsoLocal(a.last_active) : '—'} />
            </CardContent>
          </Card>

          <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
            {/* Top apps */}
            <Card>
              <CardHeader>
                <CardTitle className="text-sm">{t('apps.title')}</CardTitle>
              </CardHeader>
              <CardContent>
                <TopTable
                  rows={(q.data?.top_apps ?? []).map((r) => ({ name: r.app, count: r.samples }))}
                  nameHead={t('apps.app')}
                  countHead={t('apps.samples')}
                  empty={t('apps.empty')}
                />
              </CardContent>
            </Card>

            {/* Top sites */}
            <Card>
              <CardHeader>
                <CardTitle className="text-sm">{t('sites.title')}</CardTitle>
              </CardHeader>
              <CardContent>
                {q.data?.site_visits_capped && (
                  <div className="mb-2 text-[10px] text-amber-light">{t('sites.capped')}</div>
                )}
                <TopTable
                  rows={(q.data?.top_sites ?? []).map((r) => ({ name: r.host, count: r.visits }))}
                  nameHead={t('sites.host')}
                  countHead={t('sites.visits')}
                  empty={t('sites.empty')}
                />
              </CardContent>
            </Card>
          </div>
        </>
      )}
    </div>
  );
}

function Stat({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div>
      <div className="text-muted text-xs">{label}</div>
      <div className="text-lg font-semibold">{value}</div>
      {hint && <div className="text-muted text-[10px]">{hint}</div>}
    </div>
  );
}

function TopTable({
  rows,
  nameHead,
  countHead,
  empty,
}: {
  rows: { name: string; count: number }[];
  nameHead: string;
  countHead: string;
  empty: string;
}) {
  if (rows.length === 0) return <div className="text-muted text-sm">{empty}</div>;
  const max = Math.max(...rows.map((r) => r.count), 1);
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>{nameHead}</TableHead>
          <TableHead className="text-right">{countHead}</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((r) => (
          <TableRow key={r.name}>
            <TableCell>
              <div className="flex items-center gap-2">
                {/* Inline proportion bar for a quick visual rank. */}
                <span
                  className="inline-block h-2 rounded-sm bg-violet/40"
                  style={{ width: `${Math.max(4, (r.count / max) * 100)}px` }}
                />
                <code className="text-xs break-all">{r.name}</code>
              </div>
            </TableCell>
            <TableCell className="text-right text-muted text-xs">{r.count}</TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}
