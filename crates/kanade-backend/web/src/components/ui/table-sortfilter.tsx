import {
  Children,
  cloneElement,
  isValidElement,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
  type RefObject,
} from 'react';
import { createPortal } from 'react-dom';
import { ArrowDown, ArrowUp, ArrowUpDown, ChevronsUpDown, Filter, FilterX, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { cn } from '@/lib/utils';

/* ------------------------------------------------------------------ *
 * Excel-style column sorting + filtering for <Table sortFilter>
 *
 * Everything here is client-side over the rows the table was handed.
 * The page keeps rendering plain <TableRow>/<TableCell> children; the
 * table reads each data row's cells (in SOURCE order, before the column
 * permutation, so a stored condition keeps following its column when the
 * operator reorders), then sorts / drops whole rows before they render.
 *
 * Only rows that are direct children of <TableBody> and carry exactly one
 * cell per column count as data. Anything else — an empty-state row, a
 * group header, a Fragment wrapping a row plus its detail row — is left
 * where it was, neither sorted nor filtered.
 * ------------------------------------------------------------------ */

export type SortDir = 'asc' | 'desc';
export interface SortState {
  id: string;
  dir: SortDir;
}

/** One column's condition. Both parts apply together (AND); within
 *  `values` a row matches if its text is any of them (OR). */
export interface ColumnFilter {
  /** Case-insensitive substring. */
  contains?: string;
  /** Checklist selection: the exact cell texts that stay visible. */
  values?: string[];
}
export type Filters = Readonly<Record<string, ColumnFilter>>;

export interface ColumnFlags {
  sortable: boolean;
  filterable: boolean;
}

export interface Candidate {
  value: string;
  count: number;
}

/** Everything the header controls, the chip bar and the card-mode menu need. */
export interface SortFilterApi {
  sort: SortState | null;
  filters: Filters;
  flags: Readonly<Record<string, ColumnFlags>>;
  setSort: (id: string, dir: SortDir | null) => void;
  cycleSort: (id: string) => void;
  setFilter: (id: string, filter: ColumnFilter | undefined) => void;
  clearFilters: () => void;
  /** Distinct values of one column among the rows that pass every OTHER
   *  column's filter — the Excel behaviour. */
  candidates: (id: string) => Candidate[];
  total: number;
  shown: number;
  /** The rows are one server page; say so next to the active filters. */
  pageOnly: boolean;
}

/** Cap on checklist entries rendered at once. */
const MAX_LIST = 200;

export function parseSort(raw: unknown): SortState | null {
  if (!raw || typeof raw !== 'object') return null;
  const { id, dir } = raw as Record<string, unknown>;
  return typeof id === 'string' && (dir === 'asc' || dir === 'desc') ? { id, dir } : null;
}

export function parseFilters(raw: unknown): Filters {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
  const out: Record<string, ColumnFilter> = {};
  for (const [id, v] of Object.entries(raw as Record<string, unknown>)) {
    if (!v || typeof v !== 'object') continue;
    const { contains, values } = v as Record<string, unknown>;
    const f: ColumnFilter = {};
    if (typeof contains === 'string' && contains) f.contains = contains;
    if (Array.isArray(values)) f.values = values.filter((x): x is string => typeof x === 'string');
    if (f.contains !== undefined || f.values !== undefined) out[id] = f;
  }
  return out;
}

/** The text a node renders, for sorting / matching. Icons contribute
 *  nothing; a component that renders its text from props (rather than
 *  children) is invisible here and needs `sortValue` on its cell. */
export function textOf(node: ReactNode): string {
  if (node === null || node === undefined || typeof node === 'boolean') return '';
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(textOf).join('');
  if (isValidElement(node)) return textOf((node.props as { children?: ReactNode }).children);
  return '';
}

interface CellValue {
  sort: string | number;
  text: string;
}

interface Unit {
  node: ReactNode;
  /** `null` for a node that isn't a data row. */
  cells: CellValue[] | null;
}

function cellValue(cell: ReactNode): CellValue {
  if (!isValidElement(cell)) return { sort: '', text: '' };
  const props = cell.props as { children?: ReactNode; sortValue?: string | number };
  const text = textOf(props.children).replace(/\s+/g, ' ').trim();
  const sv = props.sortValue;
  return {
    sort: sv ?? text,
    text: text || (sv === undefined ? '' : String(sv)),
  };
}

export interface ProcessInput {
  /** The <Table>'s children: header section + body. */
  children: ReactNode;
  bodyType: unknown;
  rowType: unknown;
  /** Column ids in source order, metadata columns last. */
  ids: string[];
  /** How many of those come from the page's own cells. */
  ownCount: number;
  metaKeys: string[];
  metaByPc: Record<string, Record<string, string>>;
  filters: Filters;
  sort: SortState | null;
  flags: Record<string, ColumnFlags>;
  language: string;
  empty: () => ReactNode;
}

export interface ProcessResult {
  children: ReactNode;
  total: number;
  shown: number;
  candidates: (id: string) => Candidate[];
}

export function processSortFilter(input: ProcessInput): ProcessResult {
  const { ids, ownCount, metaKeys, metaByPc, filters, flags } = input;
  const top = Children.toArray(input.children);
  const bodyAt = top.findIndex((c) => isValidElement(c) && c.type === input.bodyType);
  const none: ProcessResult = {
    children: input.children,
    total: 0,
    shown: 0,
    candidates: () => [],
  };
  if (bodyAt < 0) return none;
  const body = top[bodyAt] as ReactElement<{ children?: ReactNode }>;

  const units: Unit[] = Children.toArray(body.props.children).map((node) => {
    if (!isValidElement(node) || node.type !== input.rowType) return { node, cells: null };
    const row = node as ReactElement<{ children?: ReactNode; pcId?: string }>;
    const own = Children.toArray(row.props.children);
    if (own.length !== ownCount) return { node, cells: null };
    const pcId = row.props.pcId;
    const cells = ids.map((_, i): CellValue => {
      if (i < ownCount) return cellValue(own[i]);
      const text = (pcId !== undefined ? metaByPc[pcId]?.[metaKeys[i - ownCount]] : undefined) ?? '';
      return { sort: text, text };
    });
    return { node, cells };
  });

  const matches = (u: Unit, skip?: string): boolean => {
    if (!u.cells) return true;
    for (const [id, f] of Object.entries(filters)) {
      const at = ids.indexOf(id);
      if (at < 0 || id === skip) continue;
      const text = u.cells[at].text;
      const q = f.contains?.trim().toLowerCase();
      if (q && !text.toLowerCase().includes(q)) return false;
      if (f.values && !f.values.includes(text)) return false;
    }
    return true;
  };

  const collator = new Intl.Collator(input.language, { numeric: true, sensitivity: 'base' });
  const data = units.filter((u) => u.cells);
  const kept = data.filter((u) => matches(u));

  const sortAt = input.sort && flags[input.sort.id]?.sortable !== false ? ids.indexOf(input.sort.id) : -1;
  if (input.sort && sortAt >= 0) {
    const sign = input.sort.dir === 'asc' ? 1 : -1;
    const blank = (v: string | number) => v === '';
    // Array#sort is stable, so ties — and the "no sort" order — are the
    // order the page gave. Blanks go last in both directions.
    kept.sort((a, b) => {
      const x = a.cells![sortAt].sort;
      const y = b.cells![sortAt].sort;
      if (blank(x) || blank(y)) return blank(x) === blank(y) ? 0 : blank(x) ? 1 : -1;
      if (typeof x === 'number' && typeof y === 'number') return sign * (x - y);
      return sign * collator.compare(String(x), String(y));
    });
  }

  // Sorted rows fill the data rows' own slots, in order; any other node
  // (empty state, group header, ...) stays exactly where it was.
  let next = 0;
  const out: ReactNode[] = [];
  for (const u of units) {
    if (!u.cells) out.push(u.node);
    else if (next < kept.length) out.push(kept[next++].node);
  }
  if (data.length > 0 && kept.length === 0) out.push(input.empty());

  const nextTop = [...top];
  nextTop[bodyAt] = cloneElement(body, undefined, ...out);

  return {
    children: nextTop,
    total: data.length,
    shown: kept.length,
    candidates: (id) => {
      const at = ids.indexOf(id);
      if (at < 0) return [];
      const counts = new Map<string, number>();
      for (const u of data) if (matches(u, id)) counts.set(u.cells![at].text, (counts.get(u.cells![at].text) ?? 0) + 1);
      return [...counts]
        .map(([value, count]) => ({ value, count }))
        .sort((a, b) =>
          a.value === '' || b.value === ''
            ? a.value === b.value
              ? 0
              : a.value === ''
                ? 1
                : -1
            : collator.compare(a.value, b.value),
        );
    },
  };
}

/* ------------------------------------------------------------------ *
 * UI
 * ------------------------------------------------------------------ */

/** A panel pinned to its trigger, rendered into <body>. Portalled so the
 *  sticky <thead> (`z-20`) and any clipping ancestor can't cut it off. */
function FloatingPanel({
  anchor,
  onClose,
  label,
  children,
}: {
  anchor: RefObject<HTMLElement | null>;
  onClose: () => void;
  label: string;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  useLayoutEffect(() => {
    const r = anchor.current?.getBoundingClientRect();
    if (!r) return;
    setPos({ top: r.bottom + 4, left: Math.max(8, Math.min(r.left, window.innerWidth - 280)) });
  }, [anchor]);

  useEffect(() => {
    const close = () => {
      onClose();
      anchor.current?.focus();
    };
    const onDown = (e: PointerEvent) => {
      const target = e.target as Node;
      if (ref.current?.contains(target) || anchor.current?.contains(target)) return;
      onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      e.stopPropagation();
      close();
    };
    const onScroll = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    document.addEventListener('pointerdown', onDown);
    document.addEventListener('keydown', onKey);
    window.addEventListener('resize', onClose);
    window.addEventListener('scroll', onScroll, true);
    return () => {
      document.removeEventListener('pointerdown', onDown);
      document.removeEventListener('keydown', onKey);
      window.removeEventListener('resize', onClose);
      window.removeEventListener('scroll', onScroll, true);
    };
  }, [anchor, onClose]);

  return createPortal(
    <div
      ref={ref}
      role="dialog"
      aria-label={label}
      style={{ position: 'fixed', top: pos?.top ?? 0, left: pos?.left ?? 0, visibility: pos ? 'visible' : 'hidden' }}
      className="z-50 max-h-[70vh] w-64 overflow-auto rounded-md border border-border bg-card p-2 text-sm normal-case tracking-normal text-fg shadow-lg"
    >
      {children}
    </div>,
    document.body,
  );
}

const SMALL_BTN =
  'inline-flex items-center gap-1 rounded border border-border px-1.5 py-0.5 text-xs hover:bg-muted/10 disabled:pointer-events-none disabled:opacity-40';
const INPUT =
  'h-7 w-full rounded border border-border bg-bg px-2 text-xs focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring';

/** The body of one column's popover: sort buttons, a "contains" box and
 *  the Excel-style value checklist. Shared by the header button and the
 *  card-mode menu. */
export function ColumnPanel({ api, colId, label }: { api: SortFilterApi; colId: string; label: string }) {
  const { t } = useTranslation('common');
  const [search, setSearch] = useState('');
  const flags = api.flags[colId] ?? { sortable: false, filterable: false };
  const filter = api.filters[colId];
  const all = flags.filterable ? api.candidates(colId) : [];
  const needle = search.trim().toLowerCase();
  const listed = all.filter((c) => c.value.toLowerCase().includes(needle));
  const isOn = (value: string) => (filter?.values ? filter.values.includes(value) : true);

  const setValues = (values: string[] | undefined) => {
    // Everything ticked is no filter at all, not a filter listing every value.
    const everything = values && all.every((c) => values.includes(c.value));
    api.setFilter(colId, { ...filter, values: everything ? undefined : values });
  };
  const toggle = (value: string) => {
    const current = filter?.values ?? all.map((c) => c.value);
    setValues(current.includes(value) ? current.filter((v) => v !== value) : [...current, value]);
  };

  return (
    <div className="space-y-2">
      {flags.sortable && (
        <div className="flex items-center gap-1" role="group" aria-label={t('table.sort.group')}>
          {(['asc', 'desc'] as const).map((dir) => (
            <button
              key={dir}
              type="button"
              aria-pressed={api.sort?.id === colId && api.sort.dir === dir}
              onClick={() => api.setSort(colId, api.sort?.id === colId && api.sort.dir === dir ? null : dir)}
              className={cn(SMALL_BTN, api.sort?.id === colId && api.sort.dir === dir && 'border-accent text-accent')}
            >
              {dir === 'asc' ? <ArrowUp className="size-3" aria-hidden /> : <ArrowDown className="size-3" aria-hidden />}
              {t(dir === 'asc' ? 'table.sort.asc' : 'table.sort.desc')}
            </button>
          ))}
        </div>
      )}
      {flags.filterable && (
        <>
          <input
            type="search"
            autoFocus
            value={filter?.contains ?? ''}
            placeholder={t('table.filter.contains')}
            aria-label={t('table.filter.containsAria', { name: label })}
            onChange={(e) => api.setFilter(colId, { ...filter, contains: e.target.value || undefined })}
            className={INPUT}
          />
          <input
            type="search"
            value={search}
            placeholder={t('table.filter.search')}
            aria-label={t('table.filter.search')}
            onChange={(e) => setSearch(e.target.value)}
            className={INPUT}
          />
          <div className="max-h-48 overflow-auto rounded border border-border p-1" role="group" aria-label={t('table.filter.values')}>
            <label className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 text-xs hover:bg-muted/10">
              <input
                type="checkbox"
                checked={!filter?.values}
                onChange={() => setValues(filter?.values ? undefined : [])}
              />
              <span className="flex-1">{t('table.filter.selectAll')}</span>
            </label>
            {listed.slice(0, MAX_LIST).map((c) => (
              <label key={c.value} className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 text-xs hover:bg-muted/10">
                <input type="checkbox" checked={isOn(c.value)} onChange={() => toggle(c.value)} />
                <span className="flex-1 truncate" title={c.value}>
                  {c.value === '' ? t('table.filter.blanks') : c.value}
                </span>
                <span className="text-muted">{c.count}</span>
              </label>
            ))}
            {listed.length > MAX_LIST && (
              <p className="px-1 py-0.5 text-[10px] text-muted">{t('table.filter.truncated', { n: MAX_LIST })}</p>
            )}
          </div>
          <button type="button" disabled={!filter} onClick={() => api.setFilter(colId, undefined)} className={SMALL_BTN}>
            <FilterX className="size-3" aria-hidden />
            {t('table.filter.clear')}
          </button>
        </>
      )}
    </div>
  );
}

/** What a header cell holds on a `sortFilter` table: the label as a sort
 *  button, plus the filter popover trigger. A column that is neither
 *  sortable nor filterable keeps its children untouched. */
export function SortFilterHeader({
  api,
  colId,
  children,
}: {
  api: SortFilterApi;
  colId: string;
  children: ReactNode;
}) {
  const { t } = useTranslation('common');
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLButtonElement>(null);
  const flags = api.flags[colId];
  if (!flags || (!flags.sortable && !flags.filterable)) return <>{children}</>;
  const label = textOf(children).trim();
  const dir = api.sort?.id === colId ? api.sort.dir : null;
  const filtered = !!api.filters[colId];
  const Indicator = dir === 'asc' ? ArrowUp : dir === 'desc' ? ArrowDown : ChevronsUpDown;

  return (
    <div className="inline-flex max-w-full items-center gap-1">
      {flags.sortable ? (
        <button
          type="button"
          onClick={() => api.cycleSort(colId)}
          title={t('table.sort.hint', { name: label })}
          className="inline-flex items-center gap-1 rounded uppercase tracking-wide hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          {children}
          <Indicator className={cn('size-3 shrink-0', !dir && 'opacity-40')} aria-hidden />
        </button>
      ) : (
        children
      )}
      {flags.filterable && (
        <>
          <button
            ref={anchor}
            type="button"
            aria-label={t('table.filter.button', { name: label })}
            aria-haspopup="dialog"
            aria-expanded={open}
            data-filter-active={filtered || undefined}
            onClick={() => setOpen((o) => !o)}
            className={cn(
              'shrink-0 rounded p-0.5 hover:bg-muted/20 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
              filtered ? 'text-accent' : 'opacity-50 hover:opacity-100',
            )}
          >
            {filtered ? <FilterX className="size-3.5" aria-hidden /> : <Filter className="size-3.5" aria-hidden />}
          </button>
          {open && (
            <FloatingPanel anchor={anchor} onClose={() => setOpen(false)} label={label}>
              <ColumnPanel api={api} colId={colId} label={label} />
            </FloatingPanel>
          )}
        </>
      )}
    </div>
  );
}

function summarise(f: ColumnFilter, blanks: string): string {
  const parts: string[] = [];
  if (f.contains?.trim()) parts.push(`"${f.contains.trim()}"`);
  if (f.values) {
    const shown = f.values.slice(0, 3).map((v) => v || blanks);
    parts.push(shown.join(', ') + (f.values.length > 3 ? ` +${f.values.length - 3}` : '') || '∅');
  }
  return parts.join(' + ');
}

/**
 * The strip above the table: one chip per active filter (including columns
 * hidden by the picker, so an invisible condition can still be seen and
 * cleared), "clear all", the shown / total count, and — below the card
 * breakpoint, where there is no header row — a menu that opens the same
 * panel for any column.
 */
export function SortFilterBar({
  api,
  columns,
  cardMode,
}: {
  api: SortFilterApi;
  columns: readonly { id: string; label: string }[];
  cardMode: boolean;
}) {
  const { t } = useTranslation('common');
  const [open, setOpen] = useState(false);
  const [picked, setPicked] = useState<string>('');
  const anchor = useRef<HTMLButtonElement>(null);
  const active = Object.keys(api.filters);
  if (!cardMode && active.length === 0) return null;

  const labelOf = (id: string, i = -1) =>
    columns.find((c) => c.id === id)?.label || t('table.columns.unnamed', { n: i + 1 });
  const usable = columns.filter((c) => api.flags[c.id]?.sortable || api.flags[c.id]?.filterable);
  const current = usable.some((c) => c.id === picked) ? picked : (usable[0]?.id ?? '');

  return (
    <div className="flex flex-wrap items-center gap-1.5 text-xs" data-testid="table-filter-bar">
      {cardMode && usable.length > 0 && (
        <>
          <button
            ref={anchor}
            type="button"
            aria-haspopup="dialog"
            aria-expanded={open}
            onClick={() => setOpen((o) => !o)}
            className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm hover:bg-muted/10"
          >
            <ArrowUpDown className="size-3.5" aria-hidden />
            {t('table.filter.menu')}
          </button>
          {open && (
            <FloatingPanel anchor={anchor} onClose={() => setOpen(false)} label={t('table.filter.menu')}>
              <select
                value={current}
                aria-label={t('table.filter.column')}
                onChange={(e) => setPicked(e.target.value)}
                className={cn(INPUT, 'mb-2')}
              >
                {usable.map((c, i) => (
                  <option key={c.id} value={c.id}>
                    {c.label || t('table.columns.unnamed', { n: i + 1 })}
                  </option>
                ))}
              </select>
              <ColumnPanel key={current} api={api} colId={current} label={labelOf(current)} />
            </FloatingPanel>
          )}
        </>
      )}
      {active.map((id) => (
        <span
          key={id}
          data-testid="filter-chip"
          className="inline-flex items-center gap-1 rounded-full border border-accent/40 bg-accent/10 py-0.5 pl-2 pr-1"
        >
          <span className="max-w-64 truncate">
            {labelOf(id)}: {summarise(api.filters[id], t('table.filter.blanks'))}
          </span>
          <button
            type="button"
            aria-label={t('table.filter.remove', { name: labelOf(id) })}
            onClick={() => api.setFilter(id, undefined)}
            className="rounded-full p-0.5 hover:bg-muted/20"
          >
            <X className="size-3" aria-hidden />
          </button>
        </span>
      ))}
      {active.length > 0 && (
        <>
          <button type="button" onClick={api.clearFilters} className="text-muted hover:text-fg">
            {t('table.filter.clearAll')}
          </button>
          <span className="text-muted" data-testid="filter-count">
            {t('table.filter.count', { shown: api.shown, total: api.total })}
          </span>
          {api.pageOnly && <span className="text-muted">{t('table.filter.pageOnly')}</span>}
        </>
      )}
    </div>
  );
}
