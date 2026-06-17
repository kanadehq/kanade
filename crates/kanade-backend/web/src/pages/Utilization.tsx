import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { PcPicker } from '@/components/PcPicker';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

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
  top_apps: { app: string; samples: number; est_minutes: number }[];
  top_sites: { host: string; visits: number }[];
  site_visits_capped: boolean;
  timeline: { hour: number; total: number; active: number }[];
};

// Local calendar day → UTC [from, to) bounds, so the day boundary is the
// operator's, not UTC's. setDate(+1) (not +86_400_000 ms) so DST change
// days still land on the next calendar midnight. Returns null for an
// empty/invalid date (a cleared picker) so we don't call toISOString on
// an Invalid Date and crash (coderabbit).
function dayBounds(date: string): { from: string; to: string } | null {
  if (!date) return null;
  const start = new Date(`${date}T00:00:00`);
  if (Number.isNaN(start.getTime())) return null;
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
  const [pcId, setPcId] = useState('');
  const [date, setDate] = useState(todayLocal());

  const bounds = useMemo(() => dayBounds(date), [date]);
  // Minutes to ADD to UTC to get local time (JST = +540), for hour-of-
  // day bucketing. Computed for the SELECTED day (not "now") so a
  // historical date with a different DST status buckets correctly.
  const tzOffset = useMemo(
    () => (date ? -new Date(`${date}T00:00:00`).getTimezoneOffset() : 0),
    [date],
  );

  const q = useQuery({
    queryKey: ['utilization', pcId, bounds?.from, bounds?.to, tzOffset],
    queryFn: () => {
      if (!bounds) throw new Error('no date bounds');
      return apiFetch<UtilizationResponse>(
        `/api/utilization/${encodeURIComponent(pcId)}?from=${encodeURIComponent(bounds.from)}&to=${encodeURIComponent(bounds.to)}&tz_offset_minutes=${tzOffset}`,
      );
    },
    enabled: !!pcId && !!bounds,
  });

  const a = q.data?.active;

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <h2 className="text-xl">{t('title')}</h2>
        <div className="flex flex-wrap items-end gap-3">
          <div className="space-y-1">
            <Label htmlFor="util-pc">{t('controls.pc')}</Label>
            <PcPicker id="util-pc" value={pcId} onChange={setPcId} className="w-56" />
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

      {!pcId ? (
        <div className="text-muted text-sm">{t('selectPc')}</div>
      ) : q.isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />
          {t('loading')}
        </div>
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

          {/* Hourly active/idle timeline */}
          <Card>
            <CardHeader>
              <CardTitle className="text-sm">{t('timeline.title')}</CardTitle>
            </CardHeader>
            <CardContent>
              <TimelineStrip
                timeline={q.data?.timeline ?? []}
                activeLabel={t('timeline.active')}
                idleLabel={t('timeline.idle')}
                noDataLabel={t('timeline.noData')}
              />
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
                  rows={(q.data?.top_apps ?? []).map((r) => ({ name: r.app, count: r.est_minutes }))}
                  nameHead={t('apps.app')}
                  countHead={t('apps.time')}
                  empty={t('apps.empty')}
                  fmtValue={fmtMinutes}
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
  fmtValue,
}: {
  rows: { name: string; count: number }[];
  nameHead: string;
  countHead: string;
  empty: string;
  fmtValue?: (n: number) => string;
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
            <TableCell className="text-right text-muted text-xs">
              {fmtValue ? fmtValue(r.count) : r.count}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

// 24-hour active/idle strip. Each hour is a column whose violet fill is
// the active proportion of that hour's presence samples; hours with no
// samples render as an empty dashed slot.
function TimelineStrip({
  timeline,
  activeLabel,
  idleLabel,
  noDataLabel,
}: {
  timeline: { hour: number; total: number; active: number }[];
  activeLabel: string;
  idleLabel: string;
  noDataLabel: string;
}) {
  const byHour = new Map(timeline.map((b) => [b.hour, b]));
  const hours = Array.from({ length: 24 }, (_, h) => h);
  return (
    <div>
      <div className="flex h-24 items-end gap-[2px]">
        {hours.map((h) => {
          const b = byHour.get(h);
          const total = b?.total ?? 0;
          const active = b?.active ?? 0;
          const activePct = total > 0 ? (active / total) * 100 : 0;
          const title =
            total > 0 ? `${h}:00 — ${activeLabel} ${active}/${total}` : `${h}:00 — ${noDataLabel}`;
          return (
            <div key={h} className="flex h-full flex-1 flex-col justify-end" title={title}>
              {total > 0 ? (
                <div className="flex h-full w-full flex-col justify-end overflow-hidden rounded-sm bg-muted/20">
                  <div className="w-full bg-violet/60" style={{ height: `${activePct}%` }} />
                </div>
              ) : (
                <div className="h-full w-full rounded-sm border border-dashed border-border/40" />
              )}
            </div>
          );
        })}
      </div>
      <div className="mt-1 flex gap-[2px] text-[9px] text-muted">
        {hours.map((h) => (
          <div key={h} className="flex-1 text-center">
            {h % 6 === 0 ? h : ''}
          </div>
        ))}
      </div>
      <div className="mt-2 flex gap-4 text-[10px] text-muted">
        <span className="flex items-center gap-1">
          <span className="inline-block size-2 rounded-sm bg-violet/60" />
          {activeLabel}
        </span>
        <span className="flex items-center gap-1">
          <span className="inline-block size-2 rounded-sm bg-muted/20" />
          {idleLabel}
        </span>
      </div>
    </div>
  );
}
