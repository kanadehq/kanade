import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { OperationalTimeline, type OpEvent } from '@/components/OperationalTimeline';
import { PcPicker } from '@/components/PcPicker';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Table, TableBody, TableCell, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

// Mirrors the backend `WidgetResult` (api/analytics.rs): every job's
// `aggregate:` widgets, computed per scope, tagged by `render`.
type BarRow = { label: string; value: number; est_minutes?: number };
type HourBucket = { hour: number; total: number; active: number };
type Widget = {
  dashboard: string;
  title: string;
  description?: string;
  scope: 'pc' | 'fleet';
} & (
  | { render: 'bar'; rows: BarRow[] }
  | {
      render: 'gauge';
      total: number;
      active: number;
      ratio: number;
      est_minutes?: number;
      first?: string;
      last?: string;
    }
  | { render: 'timeline'; metric: 'ratio' | 'count'; buckets: HourBucket[] }
  | { render: 'stat'; value: number; est_minutes?: number }
  | { render: 'op_timeline'; from: string; to: string; events: OpEvent[] }
);

type Scope = 'fleet' | 'pc';

// Local calendar range → UTC [from, to) bounds: `to` is the inclusive
// last day, so the exclusive upper bound is its midnight + 1.
// setDate(+1) keeps DST-change days on
// the next calendar midnight; swaps if from > to; null on a cleared date.
function rangeBounds(fromDate: string, toDate: string): { from: string; to: string } | null {
  if (!fromDate || !toDate) return null;
  let start = new Date(`${fromDate}T00:00:00`);
  let endExclusive = new Date(`${toDate}T00:00:00`);
  if (Number.isNaN(start.getTime()) || Number.isNaN(endExclusive.getTime())) return null;
  if (endExclusive < start) [start, endExclusive] = [endExclusive, start];
  endExclusive.setDate(endExclusive.getDate() + 1);
  return { from: start.toISOString(), to: endExclusive.toISOString() };
}

function todayLocal(): string {
  const d = new Date();
  const off = d.getTimezoneOffset() * 60_000;
  return new Date(d.getTime() - off).toISOString().slice(0, 10);
}

function addDaysLocal(date: string, days: number): string {
  const d = new Date(`${date}T00:00:00`);
  d.setDate(d.getDate() + days);
  const off = d.getTimezoneOffset() * 60_000;
  return new Date(d.getTime() - off).toISOString().slice(0, 10);
}

const PRESETS: { key: string; days: number }[] = [
  { key: 'today', days: 1 },
  { key: 'last7', days: 7 },
  { key: 'last30', days: 30 },
];

function fmtMinutes(min: number): string {
  // est_minutes is typed `number` (the backend could send a float), so
  // floor the remainder rather than print `30.5m`.
  const h = Math.floor(min / 60);
  const m = Math.floor(min % 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

export function Analytics() {
  const { t } = useTranslation('analytics');
  const [scope, setScope] = useState<Scope>('fleet');
  const [pcId, setPcId] = useState('');
  const [fromDate, setFromDate] = useState(todayLocal());
  const [toDate, setToDate] = useState(todayLocal());
  const [tab, setTab] = useState<string | null>(null);

  const today = useMemo(() => todayLocal(), []);
  const bounds = useMemo(() => rangeBounds(fromDate, toDate), [fromDate, toDate]);
  const tzOffset = useMemo(() => {
    if (!fromDate) return 0;
    const d = new Date(`${fromDate}T00:00:00`);
    // A partial/invalid date would make getTimezoneOffset() return NaN —
    // fall back to 0 (the query is gated on `bounds` anyway).
    return Number.isNaN(d.getTime()) ? 0 : -d.getTimezoneOffset();
  }, [fromDate]);

  // Keep the inputs a valid range — drag one end past the other and it
  // pulls the other along (no silent from > to on screen).
  function handleFrom(v: string) {
    setFromDate(v);
    if (v && toDate && v > toDate) setToDate(v);
  }
  function handleTo(v: string) {
    setToDate(v);
    if (v && fromDate && v < fromDate) setFromDate(v);
  }
  function applyPreset(days: number) {
    setFromDate(addDaysLocal(today, -(days - 1)));
    setToDate(today);
  }

  // Fleet scope omits pc_id (backend computes the fleet widgets); per-PC
  // scope needs a chosen PC before it queries.
  const ready = !!bounds && (scope === 'fleet' || !!pcId);

  const q = useQuery({
    // pc_id only varies the result in per-PC scope; drop it from the key
    // in fleet scope so a stale pcId doesn't fragment the fleet cache.
    queryKey: ['analytics', scope, scope === 'pc' ? pcId : null, bounds?.from, bounds?.to, tzOffset],
    queryFn: () => {
      if (!bounds) throw new Error('no date bounds');
      const params = new URLSearchParams({
        from: bounds.from,
        to: bounds.to,
        tz_offset_minutes: String(tzOffset),
      });
      if (scope === 'pc') params.set('pc_id', pcId);
      return apiFetch<Widget[]>(`/api/analytics?${params.toString()}`);
    },
    enabled: ready,
  });

  const widgets = q.data ?? [];
  // Backend returns widgets sorted by dashboard then title, so first-seen
  // order is the tab order.
  const dashboards = [...new Set(widgets.map((w) => w.dashboard))];
  const activeTab = tab && dashboards.includes(tab) ? tab : (dashboards[0] ?? null);
  const shown = widgets.filter((w) => w.dashboard === activeTab);

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <h2 className="text-xl">{t('title')}</h2>
        <div className="flex flex-wrap items-end gap-3">
          <div className="space-y-1">
            <Label>{t('controls.scope')}</Label>
            <div className="flex gap-1">
              <Button
                type="button"
                variant={scope === 'fleet' ? 'default' : 'secondary'}
                size="sm"
                onClick={() => setScope('fleet')}
              >
                {t('controls.fleet')}
              </Button>
              <Button
                type="button"
                variant={scope === 'pc' ? 'default' : 'secondary'}
                size="sm"
                onClick={() => setScope('pc')}
              >
                {t('controls.perPc')}
              </Button>
            </div>
          </div>
          {scope === 'pc' && (
            <div className="space-y-1">
              <Label htmlFor="an-pc">{t('controls.pc')}</Label>
              <PcPicker id="an-pc" value={pcId} onChange={setPcId} className="w-56" />
            </div>
          )}
          <div className="space-y-1">
            <Label htmlFor="an-from">{t('controls.from')}</Label>
            <Input
              id="an-from"
              type="date"
              value={fromDate}
              max={today}
              onChange={(e) => handleFrom(e.target.value)}
              className="w-40"
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="an-to">{t('controls.to')}</Label>
            <Input
              id="an-to"
              type="date"
              value={toDate}
              min={fromDate || undefined}
              max={today}
              onChange={(e) => handleTo(e.target.value)}
              className="w-40"
            />
          </div>
          <div className="flex items-center gap-1 pb-0.5">
            {PRESETS.map((p) => (
              <Button
                key={p.key}
                type="button"
                variant="secondary"
                size="sm"
                onClick={() => applyPreset(p.days)}
              >
                {t(`controls.presets.${p.key}`)}
              </Button>
            ))}
          </div>
        </div>
      </div>

      {/* Dashboard tabs (only when more than one is declared). */}
      {dashboards.length > 1 && (
        <div className="flex flex-wrap gap-1 border-b border-border">
          {dashboards.map((d) => (
            <button
              key={d}
              type="button"
              onClick={() => setTab(d)}
              className={`-mb-px border-b-2 px-3 py-1.5 text-sm ${
                d === activeTab
                  ? 'border-accent text-fg'
                  : 'border-transparent text-muted hover:text-fg'
              }`}
            >
              {d}
            </button>
          ))}
        </div>
      )}

      {scope === 'pc' && !pcId ? (
        <div className="text-muted text-sm">{t('selectPc')}</div>
      ) : !bounds ? (
        <div className="text-muted text-sm">{t('selectDates')}</div>
      ) : q.isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />
          {t('loading')}
        </div>
      ) : q.error ? (
        <ErrorCard title={t('errorTitle')} error={q.error} />
      ) : shown.length === 0 ? (
        <div className="text-muted text-sm">{t('empty')}</div>
      ) : (
        <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
          {shown.map((w, i) => (
            <WidgetCard key={`${w.dashboard}:${w.title}:${i}`} w={w} t={t} />
          ))}
        </div>
      )}
    </div>
  );
}

function WidgetCard({ w, t }: { w: Widget; t: (k: string) => string }) {
  // Timeline, bar and the operational swimlane read better full-width.
  const span =
    w.render === 'timeline' || w.render === 'bar' || w.render === 'op_timeline'
      ? 'lg:col-span-2'
      : '';
  return (
    <Card className={span}>
      <CardHeader>
        <CardTitle className="text-sm">{w.title}</CardTitle>
        {w.description?.trim() && <p className="mt-1 text-muted text-xs">{w.description}</p>}
      </CardHeader>
      <CardContent>
        {w.render === 'bar' && <BarTable rows={w.rows} empty={t('noData')} />}
        {w.render === 'gauge' && <Gauge w={w} t={t} />}
        {w.render === 'timeline' && (
          <TimelineStrip
            metric={w.metric}
            buckets={w.buckets}
            activeLabel={t('timeline.active')}
            idleLabel={t('timeline.idle')}
            countLabel={t('timeline.count')}
            noDataLabel={t('timeline.noData')}
          />
        )}
        {w.render === 'stat' && (
          <div>
            <div className="text-2xl font-semibold">{w.value.toLocaleString()}</div>
            {w.est_minutes != null && (
              <div className="text-muted text-xs">≈ {fmtMinutes(w.est_minutes)}</div>
            )}
          </div>
        )}
        {w.render === 'op_timeline' && (
          <OperationalTimeline events={w.events} from={w.from} to={w.to} />
        )}
      </CardContent>
    </Card>
  );
}

// Ranked bars. When rows carry `est_minutes` (a sampled count → time),
// the value column shows the estimated time; otherwise the raw value.
function BarTable({ rows, empty }: { rows: BarRow[]; empty: string }) {
  if (rows.length === 0) return <div className="text-muted text-sm">{empty}</div>;
  const useTime = rows.some((r) => r.est_minutes != null);
  const max = Math.max(...rows.map((r) => r.value), 1);
  return (
    <Table>
      <TableBody>
        {rows.map((r) => (
          <TableRow key={r.label}>
            <TableCell>
              <div className="flex items-center gap-2">
                <span
                  className="inline-block h-2 rounded-sm bg-violet/40"
                  style={{ width: `${Math.max(4, (r.value / max) * 100)}px` }}
                />
                <code className="text-xs break-all">{r.label}</code>
              </div>
            </TableCell>
            <TableCell className="text-right text-muted text-xs">
              {useTime && r.est_minutes != null ? fmtMinutes(r.est_minutes) : r.value.toLocaleString()}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

function Gauge({
  w,
  t,
}: {
  w: Extract<Widget, { render: 'gauge' }>;
  t: (k: string) => string;
}) {
  return (
    <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
      <Stat label={t('gauge.ratio')} value={`${Math.round(w.ratio * 100)}%`} />
      {w.est_minutes != null && (
        <Stat label={t('gauge.estTime')} value={fmtMinutes(w.est_minutes)} />
      )}
      <Stat label={t('gauge.first')} value={w.first ? fmtIsoLocal(w.first) : '—'} />
      <Stat label={t('gauge.last')} value={w.last ? fmtIsoLocal(w.last) : '—'} />
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-muted text-xs">{label}</div>
      <div className="text-lg font-semibold">{value}</div>
    </div>
  );
}

// 24-hour strip with two modes (driven by the backend `metric`):
//   - `ratio` (presence): each hour's bar is full height, the violet fill is
//     the active proportion of that hour's samples (active / total).
//   - `count` (volume, e.g. boot / logon): each hour's bar HEIGHT is scaled
//     by that hour's count relative to the busiest hour, so magnitude is
//     visible instead of every populated hour filling to the top — the old
//     strip mislabelled these as "active/idle" and flattened all magnitude.
// Hours with no samples render as an empty dashed slot either way, so "no
// data" (PC off / asleep) stays distinct from a real zero/idle hour.
function TimelineStrip({
  metric,
  buckets,
  activeLabel,
  idleLabel,
  countLabel,
  noDataLabel,
}: {
  metric: 'ratio' | 'count';
  buckets: HourBucket[];
  activeLabel: string;
  idleLabel: string;
  countLabel: string;
  noDataLabel: string;
}) {
  const byHour = new Map(buckets.map((b) => [b.hour, b]));
  const hours = Array.from({ length: 24 }, (_, h) => h);
  // Count mode normalises bar height to the busiest hour (≥1 so an empty
  // strip doesn't divide by zero). Unused in ratio mode.
  const maxTotal = Math.max(...buckets.map((b) => b.total), 1);
  return (
    <div>
      <div className="flex h-24 items-end gap-[2px]">
        {hours.map((h) => {
          const b = byHour.get(h);
          const total = b?.total ?? 0;
          const active = b?.active ?? 0;
          // ratio → fill height is the active proportion; count → fill height
          // is the hour's magnitude relative to the busiest hour.
          const fillPct =
            metric === 'ratio'
              ? total > 0
                ? (active / total) * 100
                : 0
              : (total / maxTotal) * 100;
          const title =
            total === 0
              ? `${h}:00 — ${noDataLabel}`
              : metric === 'ratio'
                ? `${h}:00 — ${activeLabel} ${active}/${total}`
                : `${h}:00 — ${countLabel} ${total}`;
          return (
            <div key={h} className="flex h-full flex-1 flex-col justify-end" title={title}>
              {total > 0 ? (
                <div className="flex h-full w-full flex-col justify-end overflow-hidden rounded-sm bg-muted/20">
                  <div
                    className="w-full bg-violet/60"
                    style={{ height: `${Math.max(fillPct, metric === 'count' ? 4 : 0)}%` }}
                  />
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
        {metric === 'ratio' ? (
          <>
            <span className="flex items-center gap-1">
              <span className="inline-block size-2 rounded-sm bg-violet/60" />
              {activeLabel}
            </span>
            <span className="flex items-center gap-1">
              <span className="inline-block size-2 rounded-sm bg-muted/20" />
              {idleLabel}
            </span>
          </>
        ) : (
          <span className="flex items-center gap-1">
            <span className="inline-block size-2 rounded-sm bg-violet/60" />
            {countLabel}
          </span>
        )}
      </div>
    </div>
  );
}
