// Reusable renderer for an Analytics aggregate widget. Extracted from the
// Analytics page so the Dashboard can render the same widgets for its
// "pinned" section (a `pin_dashboard: true` widget surfaces up front). The
// page owns scope/date controls; this module owns the per-widget visuals.

import { Cell, Pie, PieChart, ResponsiveContainer, Tooltip } from 'recharts';

import { OperationalTimeline, type OpEvent } from '@/components/OperationalTimeline';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { fmtIsoLocal } from '@/lib/utils';

// Mirrors the backend `WidgetResult` (api/analytics.rs): every job's /
// view's `aggregate:` widgets, computed per scope, tagged by `render`.
export type BarRow = { label: string; value: number; est_minutes?: number };
export type HourBucket = { hour: number; total: number; active: number };
/** One cell of a `table` widget — a JSON scalar from the SQL result. */
export type CellValue = string | number | boolean | null;
export type Widget = {
  dashboard: string;
  title: string;
  description?: string;
  scope: 'pc' | 'fleet';
  /** Whether this widget is promoted to the main Dashboard. */
  pin_dashboard?: boolean;
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
  // #vuln-roadmap PR3: SQL-backed `view:` widgets. The backend maps the SQL
  // result onto these two new shapes; the others reuse the existing renderers.
  | { render: 'table'; columns: string[]; rows: CellValue[][] }
  | { render: 'pie'; rows: BarRow[]; donut?: boolean }
);

export function fmtMinutes(min: number): string {
  // est_minutes is typed `number` (the backend could send a float), so
  // floor the remainder rather than print `30.5m`.
  const h = Math.floor(min / 60);
  const m = Math.floor(min % 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

export function WidgetCard({ w, t }: { w: Widget; t: (k: string) => string }) {
  // Timeline, bar, the operational swimlane and the (often wide) SQL table
  // read better full-width.
  const span =
    w.render === 'timeline' ||
    w.render === 'bar' ||
    w.render === 'op_timeline' ||
    w.render === 'table'
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
        {w.render === 'table' && (
          <ResultTable columns={w.columns} rows={w.rows} empty={t('noData')} />
        )}
        {w.render === 'pie' && <PieWidget rows={w.rows} donut={w.donut} empty={t('noData')} />}
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

// #vuln-roadmap PR3: the full result grid of a SQL-backed `view:` widget. The
// backend already selected + relabelled the columns; we just render the cells.
function ResultTable({
  columns,
  rows,
  empty,
}: {
  columns: string[];
  rows: CellValue[][];
  empty: string;
}) {
  if (rows.length === 0) return <div className="text-muted text-sm">{empty}</div>;
  return (
    <div className="overflow-x-auto">
      <Table>
        <TableHeader>
          <TableRow>
            {columns.map((c, ci) => (
              // Index-qualified: a SQL result can repeat a column name
              // (e.g. `SELECT a, a` or a join) — keep keys unique.
              <TableHead key={`${c}-${ci}`} className="text-xs">
                {c}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row, ri) => (
            // Row order is the query's; index is a stable key within one result.
            <TableRow key={ri}>
              {row.map((cell, ci) => (
                <TableCell key={ci} className="text-xs">
                  {cell === null ? (
                    <span className="text-muted">—</span>
                  ) : typeof cell === 'number' ? (
                    cell.toLocaleString()
                  ) : (
                    String(cell)
                  )}
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  );
}

// #vuln-roadmap PR3: parts-of-a-whole for a SQL-backed `view:` widget. `donut`
// leaves a hole with the total in the centre. Palette cycles a small violet
// ramp so slices stay on-theme and legible.
const PIE_COLORS = ['#8b5cf6', '#a78bfa', '#c4b5fd', '#7c3aed', '#6d28d9', '#ddd6fe'];

function PieWidget({ rows, donut, empty }: { rows: BarRow[]; donut?: boolean; empty: string }) {
  if (rows.length === 0) return <div className="text-muted text-sm">{empty}</div>;
  const total = rows.reduce((sum, r) => sum + r.value, 0);
  return (
    <div className="relative">
      <ResponsiveContainer width="100%" height={220}>
        <PieChart>
          <Pie
            data={rows}
            dataKey="value"
            nameKey="label"
            cx="50%"
            cy="50%"
            outerRadius={90}
            innerRadius={donut ? 55 : 0}
            paddingAngle={rows.length > 1 ? 2 : 0}
          >
            {rows.map((r, i) => (
              // Label-qualified by index: multiple rows can share a label
              // (e.g. several NULL groups rendered as "—").
              <Cell key={`${r.label}-${i}`} fill={PIE_COLORS[i % PIE_COLORS.length]} />
            ))}
          </Pie>
          <Tooltip
            formatter={(value, name) => [Number(value).toLocaleString(), String(name)]}
            contentStyle={{ fontSize: '12px' }}
          />
        </PieChart>
      </ResponsiveContainer>
      {donut && (
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
          <div className="text-center">
            <div className="text-lg font-semibold">{total.toLocaleString()}</div>
          </div>
        </div>
      )}
    </div>
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
