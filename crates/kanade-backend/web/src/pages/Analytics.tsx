import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { WidgetCard, type Widget } from '@/components/AnalyticsWidget';
import { ErrorCard } from '@/components/ErrorCard';
import { PcPicker } from '@/components/PcPicker';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiFetch } from '@/lib/api';

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
