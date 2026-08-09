import {
  Children,
  cloneElement,
  createContext,
  forwardRef,
  isValidElement,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useId,
  useRef,
  useState,
  useSyncExternalStore,
  type ForwardedRef,
  type HTMLAttributes,
  type MutableRefObject,
  type PointerEvent as ReactPointerEvent,
  type ReactElement,
  type ReactNode,
  type TdHTMLAttributes,
  type ThHTMLAttributes,
} from 'react';
import { ChevronDown, ChevronUp, Plus, RotateCcw, SlidersHorizontal } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { useQuery } from '@tanstack/react-query';

import { apiFetch } from '@/lib/api';
import { useMediaQuery } from '@/lib/hooks';
import { cn } from '@/lib/utils';

/* ------------------------------------------------------------------ *
 * Column resizing (#1344)
 *
 * Opt-in per table via `<Table resizeKey="agents">`. The key names the
 * localStorage bucket the widths persist in, per browser — the same
 * flavour of preference as the Agents column picker (`agents.hiddenColumns`).
 *
 * Three constraints from the surrounding design shaped this:
 *
 *  1. Below the card breakpoint the table is not a table at all — the
 *     rows become stacked cards and <thead> is hidden (index.css). So
 *     every part of this (handles, <colgroup>, inline widths) is gated on
 *     a JS media query rather than CSS: an inline `width` would out-specify
 *     the card-mode stylesheet and blow the cards out to the table's width.
 *  2. #1005 removed the wrapper's `overflow-x` so the sticky <thead>
 *     resolves against the viewport. We keep that: a table widened past
 *     its container grows the *wrapper* (`width: max-content`) and the
 *     whole page scrolls sideways. No nested scrollbar, sticky header
 *     intact, and nothing renders outside the card border.
 *  3. An untouched table must look exactly as it did. So the widths map
 *     starts empty and the table keeps automatic layout. A resize writes
 *     only the column it touched; a reconciliation effect then measures
 *     every *other* column where it currently sits and folds those in
 *     before the switch to `table-layout: fixed` — which is what makes the
 *     switch invisible and confines the drag to the column the operator
 *     grabbed. The same effect covers a column re-added later by a column
 *     picker, which would otherwise render at zero width.
 * ------------------------------------------------------------------ */

/** Smallest a column may be dragged to. Below this the header label is
 *  unreadable and the handle starts to overlap its neighbour's. */
const MIN_COL_WIDTH = 56;
/** Keyboard nudge per arrow press (Shift = coarse). */
const NUDGE_STEP = 16;
const NUDGE_STEP_COARSE = 64;
/** Width given to a column that first appears while the table is already
 *  in fixed layout, where it cannot be measured meaningfully. */
const DEFAULT_NEW_COL_WIDTH = 120;
const WIDTHS_PREFIX = 'kanade.table.widths.';
const HIDDEN_PREFIX = 'kanade.table.hidden.';
const ORDER_PREFIX = 'kanade.table.order.';
const META_PREFIX = 'kanade.table.meta.';
/** Persisted widths, keyed by column id (see `columnIdOf`). */
type ColumnWidths = Record<string, number>;

interface ResizeContextValue {
  /** Widths set by the operator. Empty ⇒ automatic layout, untouched. */
  widths: ColumnWidths;
  /** Column ids in render order + their total, derived from the live
   *  <thead>. `null` until widths exist, which is also the signal to
   *  render no `<colgroup>` and no inline table width. */
  layout: { order: string[]; total: number } | null;
  /** Begin a pointer drag from a header cell. */
  startResize: (th: HTMLTableCellElement, clientX: number) => void;
  /** Keyboard equivalent: widen (`dx > 0`) or narrow one column. */
  nudge: (th: HTMLTableCellElement, dx: number) => void;
  /** Drop every stored width, back to automatic layout. */
  reset: () => void;
  /** False when the table didn't opt in, or is currently rendering as
   *  cards. TableHead renders no handle at all in that case. */
  active: boolean;
  /**
   * Display position → source position (#1353 phase 2). `null` means "no
   * reordering", which is both the default and the fallback when the
   * table's header isn't shaped the way `sourceColumnIds` expects — every
   * <TableRow> then renders its cells untouched.
   *
   * Applied by position rather than by id: only the header cells carry
   * ids, and a body row has to end up in the same order as the header it
   * belongs to.
   */
  perm: number[] | null;
  /** Source-order column ids, so the header row can stamp each cell with a
   *  reorder-stable identity. Empty when the header couldn't be read. */
  sourceIds: string[];
  /**
   * `agent_meta` columns the operator has added (#1357), in the order they
   * were chosen. Appended to every row by <TableRow>: the header row gets
   * the headings, a row with a `pcId` gets that PC's values.
   */
  metaKeys: string[];
  /** `agent_meta` values for the rows on screen, by pc_id. A PC with no
   *  attributes is simply absent. */
  metaByPc: Record<string, Record<string, string>>;
  /**
   * Register a row's pc_id for the metadata fetch; the returned function
   * deregisters it.
   *
   * The rows announce themselves rather than the page passing a list,
   * because a page doesn't always have one: Compliance appends its "ok"
   * rows from a child component that fetches them itself, so the parent
   * cannot enumerate what is on screen. It also removes a second place to
   * get the pc_id expression wrong — the list and the rows can't disagree
   * when there is only one of them.
   *
   * Returns a no-op when the table isn't using metadata columns, so an
   * ordinary table pays nothing for the plumbing.
   */
  registerPc: (pcId: string) => () => void;
}

const ResizeContext = createContext<ResizeContextValue | null>(null);

/**
 * Keep a local ref AND honour the `ref` the caller forwarded. Both `Table`
 * and `TableHead` need their own DOM node (to walk the header cells / to
 * measure the cell being dragged) while staying `forwardRef` components.
 */
function useMergedRef<T>(
  local: MutableRefObject<T | null>,
  forwarded: ForwardedRef<T>,
): (node: T | null) => void {
  return useCallback(
    (node: T | null) => {
      local.current = node;
      if (typeof forwarded === 'function') forwarded(node);
      else if (forwarded) forwarded.current = node;
    },
    [local, forwarded],
  );
}

/**
 * Fallback identity for a header the page didn't give a `colId`.
 *
 * Deliberately includes the column COUNT. A bare index is not stable
 * across releases: inserting a column into a page's JSX shifts every index
 * after it, and stored preferences would then land on their neighbours —
 * silently, and looking for all the world like a bug in the feature.
 *
 * Folding the count in makes that impossible instead of merely unlikely.
 * Adding or removing a column changes every positional id at once, so the
 * stored entries stop matching and are ignored: widths are re-measured,
 * hidden columns come back, and the order falls back to source order.
 * Losing a layout on a release that changes a table's columns is a fair
 * price for never applying one to the wrong columns — and a page that
 * cares passes `colId`, which is immune either way.
 */
function positionalId(index: number, total: number): string {
  return `${index}/${total}`;
}

/**
 * The `agent_meta` attributes for a set of PCs, as `{ pc_id: { key: value } }`
 * (#1357).
 *
 * One request for the rows on screen, rather than teaching each of the
 * seven pc_id-keyed endpoints to join metadata itself. `undefined` ids —
 * no metadata column selected — issue no request at all, which is the
 * common case: this must cost nothing on a table nobody has added a
 * metadata column to.
 *
 * The ids are sorted into the query key so a re-render that reorders the
 * same rows is a cache hit rather than a refetch.
 */
function useAgentMeta(pcIds: string[] | undefined): Record<string, Record<string, string>> {
  const ids = pcIds && pcIds.length ? [...new Set(pcIds)].sort() : [];
  const { data } = useQuery({
    queryKey: ['agent-meta', ids],
    enabled: ids.length > 0,
    queryFn: async () => {
      const raw = await apiFetch<Record<string, { key: string; value: string }[]>>(
        `/api/agents/meta?pcs=${encodeURIComponent(ids.join(','))}`,
      );
      const out: Record<string, Record<string, string>> = {};
      for (const [pc, entries] of Object.entries(raw)) {
        out[pc] = Object.fromEntries(entries.map((e) => [e.key, e.value]));
      }
      return out;
    },
  });
  return data ?? EMPTY_META;
}

const EMPTY_META: Record<string, Record<string, string>> = Object.freeze({});

/**
 * The pc_ids of the rows currently rendered, collected from the rows
 * themselves (#1357).
 *
 * Updates are batched through a microtask: a page of rows mounts fifty
 * registrations in one pass, and one state update at the end of it is the
 * difference between one render and fifty. The set is sorted so the query
 * key is stable under a re-render that reorders the same rows.
 *
 * Counted rather than stored as a set: two rows can legitimately carry the
 * same pc_id (a per-check table lists a PC once per check), and the first
 * one to unmount must not take the other's registration with it.
 */
function useRegisteredPcIds(enabled: boolean): {
  pcIds: string[];
  registerPc: (pcId: string) => () => void;
} {
  const counts = useRef(new Map<string, number>());
  const [pcIds, setPcIds] = useState<string[]>([]);
  const pending = useRef(false);

  const flush = useCallback(() => {
    if (pending.current) return;
    pending.current = true;
    queueMicrotask(() => {
      pending.current = false;
      const next = [...counts.current.keys()].sort();
      setPcIds((prev) =>
        prev.length === next.length && prev.every((id, i) => id === next[i]) ? prev : next,
      );
    });
  }, []);

  const registerPc = useCallback(
    (pcId: string) => {
      if (!enabled) return () => {};
      const map = counts.current;
      map.set(pcId, (map.get(pcId) ?? 0) + 1);
      flush();
      return () => {
        const left = (map.get(pcId) ?? 1) - 1;
        if (left <= 0) map.delete(pcId);
        else map.set(pcId, left);
        flush();
      };
    },
    [enabled, flush],
  );

  return { pcIds, registerPc };
}

/** A column's identity for width storage: the explicit `colId` a page
 *  passed to <TableHead>, else its position (see [`positionalId`]). Pages
 *  whose column *set* varies at runtime — a column picker, manifest-driven
 *  columns — must pass `colId`, since for those the set changes without a
 *  release and the operator would lose the layout every time. */
function columnIdOf(th: HTMLTableCellElement): string {
  return th.dataset.colId ?? positionalId(th.cellIndex, th.parentElement?.children.length ?? 0);
}

/**
 * The column ids the page's JSX emits, in SOURCE order (#1353 phase 2).
 *
 * Read from the React children rather than the DOM, because the DOM is
 * already permuted — deriving source order from it would be circular. The
 * shape it expects is `<Table><TableHeader><TableRow>{cells}` , which every
 * table in the app uses; anything else returns `[]` and the table simply
 * doesn't reorder rather than reordering wrongly.
 *
 * A cell without `colId` is identified by its source position, which is
 * stable precisely because it is the *source* one.
 */
function sourceColumnIds(children: ReactNode): string[] {
  for (const child of Children.toArray(children)) {
    if (!isValidElement(child) || child.type !== TableHeader) continue;
    const rows = Children.toArray((child.props as { children?: ReactNode }).children);
    const row = rows.find((r) => isValidElement(r) && r.type === TableRow);
    if (!row || !isValidElement(row)) return [];
    const cells = Children.toArray((row.props as { children?: ReactNode }).children);
    return cells.map((cell, i) =>
      isValidElement(cell)
        ? ((cell.props as { colId?: string }).colId ?? positionalId(i, cells.length))
        : positionalId(i, cells.length),
    );
  }
  return [];
}

/**
 * Display position → source position, from the operator's stored order.
 *
 * Reconciles rather than trusting what was stored: ids that no longer
 * exist are dropped, and columns the operator has never seen are appended
 * in source order. So adding a column to a page doesn't scramble anyone's
 * layout, and removing one doesn't leave a hole. Returns `null` when the
 * result is the identity, which is the signal to do nothing at all.
 */
function permutationFor(sourceIds: string[], order: readonly string[]): number[] | null {
  if (!sourceIds.length || !order.length) return null;
  const seen = new Set<string>();
  const perm: number[] = [];
  for (const id of order) {
    const at = sourceIds.indexOf(id);
    if (at >= 0 && !seen.has(id)) {
      seen.add(id);
      perm.push(at);
    }
  }
  sourceIds.forEach((id, i) => {
    if (!seen.has(id)) perm.push(i);
  });
  return perm.every((source, display) => source === display) ? null : perm;
}

/**
 * Apply the table's column order to one row's cells (#1353 phase 2).
 *
 * Permuting React children — not the DOM. `Children.toArray` assigns each
 * cell a key, so React treats the result as a *move*: the existing nodes
 * are relocated rather than rebuilt, and any state or focus inside a cell
 * survives. Reordering the DOM directly would fight reconciliation.
 *
 * A row whose cell count doesn't match the header's is returned untouched.
 * That is the same guard the hidden-column stylesheet uses, and it is what
 * protects an empty-state row (one `colSpan` cell) and the expanded detail
 * rows on Jobs / Schedules from being permuted into nonsense.
 *
 * The header row additionally stamps each cell with its source id, so a
 * page that never passed `colId` still gets width storage keyed by
 * something that doesn't move when the columns do.
 */
function orderCells(
  children: ReactNode,
  table: ResizeContextValue | null,
  isHeader: boolean,
  pcId: string | undefined,
): ReactNode {
  if (!table || !table.sourceIds.length) return children;
  const own = Children.toArray(children);
  // #1357: metadata columns are appended here rather than by the page, so
  // the header row and the value rows can't disagree about how many there
  // are — which is what the cell-count guard below depends on. A row with
  // no `pcId` (an empty-state row) appends nothing and is left alone by
  // that same guard.
  const meta = table.metaKeys.length
    ? table.metaKeys.map((key) =>
        isHeader ? (
          <TableHead key={`meta:${key}`} colId={`meta:${key}`}>
            {key}
          </TableHead>
        ) : pcId !== undefined ? (
          <TableCell key={`meta:${key}`} label={key} className="text-muted text-xs">
            {table.metaByPc[pcId]?.[key] ?? ''}
          </TableCell>
        ) : null,
      )
    : [];
  const cells = isHeader || pcId !== undefined ? [...own, ...meta] : own;
  if (cells.length !== table.sourceIds.length) return children;
  const stamped = isHeader
    ? cells.map((cell, i) =>
        isValidElement(cell) && (cell.props as { colId?: string }).colId === undefined
          ? cloneElement(cell as ReactElement<{ colId?: string }>, { colId: table.sourceIds[i] })
          : cell,
      )
    : cells;
  return table.perm ? table.perm.map((source) => stamped[source]) : stamped;
}

/** The header cells that define the columns. Fixed layout sizes a table
 *  from its first row, and that is the row the handles live in. */
function headerCells(table: HTMLTableElement): HTMLTableCellElement[] {
  const row = table.tHead?.rows[0];
  return row ? (Array.from(row.cells) as HTMLTableCellElement[]) : [];
}

function parseWidths(raw: unknown): ColumnWidths {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
  const out: ColumnWidths = {};
  for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
    // Drop anything that isn't a usable width — a corrupted or hand-edited
    // entry would otherwise reach `<col width>` as NaN and collapse the
    // column. Not floored at MIN_COL_WIDTH: measured widths (see the
    // reconciliation effect) can legitimately be narrower, and discarding
    // one here would make the column look "missing" and get re-measured
    // under a layout it no longer matches.
    if (typeof v === 'number' && Number.isFinite(v) && v > 0) out[k] = v;
  }
  return out;
}

/* ------------------------------------------------------------------ *
 * Per-table preferences (#1344, #1353)
 *
 * A table's column preferences live in module-level stores rather than in
 * <Table>'s own state, because the controls that edit them are not inside
 * the table: the column picker sits in the page's filter row, and Settings
 * resets every table at once. A table's `useState` is not reachable from
 * either.
 *
 * There are two of these (widths and hidden columns), and #1353's second
 * phase adds a third (order), so the subscribe / persist / reset-all
 * plumbing is written once here and instantiated per preference. One store
 * per `resizeKey` within each preference, read through
 * `useSyncExternalStore` so every reader re-renders on a change.
 * ------------------------------------------------------------------ */

interface TablePref<T> {
  /** Live value for one table. */
  get(key: string): T;
  /** Replace it, persist it, and notify every reader. */
  update(key: string, next: T | ((prev: T) => T)): void;
  /** Subscribe from a component. `undefined` key ⇒ the empty value. */
  use(key: string | undefined): T;
  /** Clear it for EVERY table, including tables not currently mounted
   *  (whose value exists only in localStorage). Returns how many tables
   *  actually had something stored. */
  resetAll(): number;
  /** Every table that currently has a non-empty value, mounted or not.
   *  Lets a caller union the keys across preferences before clearing them,
   *  so "how many tables did that affect?" is answered once rather than
   *  per preference. */
  keysWithValue(): string[];
}

function makeTablePref<T>(opts: {
  prefix: string;
  /** Shared empty value. Must be a stable reference — `useSyncExternalStore`
   *  compares snapshots by identity, so a fresh one per call would loop. */
  empty: T;
  /** Validate whatever came back out of localStorage. Anything malformed,
   *  hand-edited or corrupt must degrade to `empty`, never reach the DOM. */
  parse: (raw: unknown) => T;
  isEmpty: (value: T) => boolean;
}): TablePref<T> {
  const stores = new Map<string, { value: T; listeners: Set<() => void> }>();

  const read = (key: string): T => {
    try {
      const raw = localStorage.getItem(opts.prefix + key);
      return raw ? opts.parse(JSON.parse(raw)) : opts.empty;
    } catch {
      /* blocked storage / malformed JSON — behave as if nothing was stored */
      return opts.empty;
    }
  };

  const write = (key: string, value: T) => {
    try {
      if (opts.isEmpty(value)) {
        // Remove the key rather than leaving an empty value behind, so
        // "has this table been customised?" is a plain key-exists question.
        localStorage.removeItem(opts.prefix + key);
      } else {
        localStorage.setItem(opts.prefix + key, JSON.stringify(value));
      }
    } catch {
      /* non-persistent this session; not worth surfacing */
    }
  };

  const store = (key: string) => {
    let s = stores.get(key);
    if (!s) {
      s = { value: read(key), listeners: new Set() };
      stores.set(key, s);
    }
    return s;
  };

  const update: TablePref<T>['update'] = (key, next) => {
    const s = store(key);
    const value = typeof next === 'function' ? (next as (prev: T) => T)(s.value) : next;
    if (value === s.value) return;
    s.value = value;
    write(key, value);
    for (const notify of s.listeners) notify();
  };

  return {
    get: (key) => store(key).value,
    update,
    use(key) {
      /* eslint-disable react-hooks/rules-of-hooks -- called from components */
      const subscribe = useCallback(
        (onChange: () => void) => {
          if (!key) return () => {};
          const s = store(key);
          s.listeners.add(onChange);
          return () => s.listeners.delete(onChange);
        },
        [key],
      );
      const snapshot = useCallback(() => (key ? store(key).value : opts.empty), [key]);
      return useSyncExternalStore(subscribe, snapshot);
    },
    keysWithValue() {
      const keys = new Set<string>(stores.keys());
      try {
        for (let i = 0; i < localStorage.length; i++) {
          const raw = localStorage.key(i);
          if (raw?.startsWith(opts.prefix)) keys.add(raw.slice(opts.prefix.length));
        }
      } catch {
        /* storage unreadable — fall back to the mounted tables alone */
      }
      return [...keys].filter((key) => !opts.isEmpty(store(key).value));
    },
    resetAll() {
      const keys = this.keysWithValue();
      for (const key of keys) update(key, opts.empty);
      return keys.length;
    },
  };
}

const widthPref = makeTablePref<ColumnWidths>({
  prefix: WIDTHS_PREFIX,
  empty: Object.freeze({}) as ColumnWidths,
  parse: parseWidths,
  isEmpty: (w) => Object.keys(w).length === 0,
});

const orderPref = makeTablePref<readonly string[]>({
  prefix: ORDER_PREFIX,
  empty: Object.freeze([]) as readonly string[],
  parse: (raw) => (Array.isArray(raw) ? raw.filter((x): x is string => typeof x === 'string') : []),
  isEmpty: (ids) => ids.length === 0,
});

/**
 * The `agent_meta` keys an operator has added as columns (#1357).
 *
 * Distinct from `hiddenPref`, which takes existing columns away: these
 * keys ADD columns. Nothing is selected by default and nothing ever will
 * be — the keys are whatever whoever administers the fleet decided to
 * record, so there is no key this code could sensibly pick for anyone.
 */
const metaPref = makeTablePref<readonly string[]>({
  prefix: META_PREFIX,
  empty: Object.freeze([]) as readonly string[],
  parse: (raw) => (Array.isArray(raw) ? raw.filter((x): x is string => typeof x === 'string') : []),
  isEmpty: (keys) => keys.length === 0,
});

const hiddenPref = makeTablePref<readonly string[]>({
  prefix: HIDDEN_PREFIX,
  empty: Object.freeze([]) as readonly string[],
  parse: (raw) => (Array.isArray(raw) ? raw.filter((x): x is string => typeof x === 'string') : []),
  isEmpty: (ids) => ids.length === 0,
});

/**
 * Read/clear one table's stored column widths from outside the table —
 * for a "reset widths" control that lives in the page's own chrome.
 * `hasWidths` is false until the operator resizes something, which is the
 * cue to render no control at all rather than a dead one.
 */
export function useTableWidths(resizeKey: string): { hasWidths: boolean; reset: () => void } {
  const widths = widthPref.use(resizeKey);
  const reset = useCallback(() => widthPref.update(resizeKey, {}), [resizeKey]);
  return { hasWidths: Object.keys(widths).length > 0, reset };
}

/**
 * Clear the stored widths of EVERY table, including tables not currently
 * mounted. The global escape hatch for "I made some table unreadable and
 * can't remember which page". Returns how many tables were reset.
 */
export function resetAllTableWidths(): number {
  return widthPref.resetAll();
}

/* ------------------------------------------------------------------ *
 * Column registry (#1353)
 *
 * The picker is not inside the table — it sits in the page's filter row —
 * so it cannot see the columns by itself. The table publishes them here as
 * it renders, read straight off the live <thead>, which is why a page adds
 * the picker with one line and passes no column list and no labels: the
 * labels ARE the header text, already translated.
 *
 * Not persisted. This is what's on screen right now, not a preference.
 * ------------------------------------------------------------------ */

export interface TableColumn {
  /** Stable identity — the `colId` the page gave the header cell, else its
   *  position. Also the key `hiddenPref` stores. */
  id: string;
  /** The header's own text. Empty for an icon-only header; the picker
   *  substitutes a positional name. */
  label: string;
}

const NO_COLUMNS: readonly TableColumn[] = Object.freeze([]);
const columnRegistry = new Map<string, { columns: readonly TableColumn[]; listeners: Set<() => void> }>();

function columnEntry(key: string) {
  let entry = columnRegistry.get(key);
  if (!entry) {
    entry = { columns: NO_COLUMNS, listeners: new Set() };
    columnRegistry.set(key, entry);
  }
  return entry;
}

function publishColumns(key: string, columns: TableColumn[]) {
  const entry = columnEntry(key);
  const same =
    entry.columns.length === columns.length &&
    entry.columns.every((c, i) => c.id === columns[i].id && c.label === columns[i].label);
  if (same) return;
  entry.columns = columns;
  for (const notify of entry.listeners) notify();
}

function useRegisteredColumns(key: string): readonly TableColumn[] {
  const subscribe = useCallback((onChange: () => void) => {
    const entry = columnEntry(key);
    entry.listeners.add(onChange);
    return () => entry.listeners.delete(onChange);
  }, [key]);
  const snapshot = useCallback(() => columnEntry(key).columns, [key]);
  return useSyncExternalStore(subscribe, snapshot);
}

/**
 * One table's columns and their visibility, for a picker rendered outside
 * it. Empty until the table has mounted and published its header.
 *
 * `toggle` refuses to hide the last visible column — a table with no
 * columns is not a state an operator can get out of from the picker,
 * because the picker itself would have nothing to list.
 */
export function useTableColumns(resizeKey: string): {
  /** In display order, i.e. after the operator's reordering. */
  columns: { id: string; label: string; visible: boolean }[];
  toggle: (id: string) => void;
  reset: () => void;
  hasHidden: boolean;
  /** Move one column one place left (`-1`) or right (`1`). A move off
   *  either end is a no-op rather than a wrap. */
  move: (id: string, delta: -1 | 1) => void;
  resetOrder: () => void;
  hasOrder: boolean;
} {
  const registered = useRegisteredColumns(resizeKey);
  const hidden = hiddenPref.use(resizeKey);
  const columns = registered.map((c) => ({ ...c, visible: !hidden.includes(c.id) }));
  // Packed to a string so it is a stable `useCallback` dep by value: the
  // array identity changes every render, the column list almost never does.
  const idKey = JSON.stringify(registered.map((c) => c.id));
  const toggle = useCallback(
    (id: string) =>
      hiddenPref.update(resizeKey, (prev) => {
        if (prev.includes(id)) return prev.filter((x) => x !== id);
        const next = [...prev, id];
        // Decided from `prev`, NOT from a count captured at render time:
        // two `toggle` calls in the same tick would both read the same
        // stale count and could hide every column between them. Reading the
        // store's own value makes the second call see the first.
        const ids: string[] = JSON.parse(idKey);
        if (ids.length > 0 && ids.every((c) => next.includes(c))) return prev;
        return next;
      }),
    [resizeKey, idKey],
  );
  const reset = useCallback(() => hiddenPref.update(resizeKey, []), [resizeKey]);
  const order = orderPref.use(resizeKey);
  // `registered` already arrives in DISPLAY order — it is read off the live
  // header, which the permutation has already been applied to. So a move is
  // a swap in that list, written back whole; storing the complete order
  // (rather than a diff from source) is what lets it survive a page adding
  // or removing a column later.
  const move = useCallback(
    (id: string, delta: -1 | 1) => {
      const ids: string[] = JSON.parse(idKey);
      const from = ids.indexOf(id);
      const to = from + delta;
      if (from < 0 || to < 0 || to >= ids.length) return;
      const next = [...ids];
      [next[from], next[to]] = [next[to], next[from]];
      orderPref.update(resizeKey, next);
    },
    [resizeKey, idKey],
  );
  const resetOrder = useCallback(() => orderPref.update(resizeKey, []), [resizeKey]);
  return {
    columns,
    toggle,
    reset,
    hasHidden: columns.some((c) => !c.visible),
    move,
    resetOrder,
    hasOrder: order.length > 0,
  };
}

/**
 * The `agent_meta` keys available fleet-wide, and which of them this table
 * shows as columns (#1357).
 *
 * The available keys come from the fleet, not from the table — an operator
 * can add a column for a key none of the rows on screen happen to have,
 * which is exactly how they discover that those PCs are missing it.
 */
export function useTableMetaColumns(
  resizeKey: string,
  /**
   * Whether this table offers metadata columns at all. `false` skips the
   * key request entirely — the picker calls this hook unconditionally, so
   * without the gate every table with a picker would ask the fleet for its
   * metadata keys whether or not it can use them.
   */
  enabled = true,
): {
  available: string[];
  selected: readonly string[];
  toggle: (key: string) => void;
  reset: () => void;
} {
  const selected = metaPref.use(resizeKey);
  const { data } = useQuery({
    queryKey: ['agent-meta-keys'],
    queryFn: () => apiFetch<string[]>('/api/agents/meta-keys'),
    enabled,
  });
  const toggle = useCallback(
    (key: string) =>
      metaPref.update(resizeKey, (prev) =>
        prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key],
      ),
    [resizeKey],
  );
  const reset = useCallback(() => metaPref.update(resizeKey, []), [resizeKey]);
  return { available: data ?? [], selected, toggle, reset };
}

/** Show every column of every table again, mounted or not. Companion to
 *  [`resetAllTableWidths`] for the Settings escape hatch. */
export function resetAllTableColumns(): number {
  return hiddenPref.resetAll();
}

/**
 * Put every table's columns back to their defaults — both widths and
 * visibility — and return how many tables were affected.
 *
 * The count is the UNION of the two preferences, not the larger of them: a
 * table with only a stored width and a different table with only a hidden
 * column are two tables, and reporting one would be wrong. Collected before
 * clearing, since afterwards there is nothing left to count.
 */
export function resetAllTableColumnPrefs(): number {
  const affected = new Set([
    ...widthPref.keysWithValue(),
    ...hiddenPref.keysWithValue(),
    ...orderPref.keysWithValue(),
  ]);
  widthPref.resetAll();
  hiddenPref.resetAll();
  orderPref.resetAll();
  return affected.size;
}

interface TableProps extends HTMLAttributes<HTMLTableElement> {
  /**
   * Enable the below-`lg` row→card transform (index.css `.kn-table`).
   * Default `true` for the app's data tables. Pass `false` for small
   * widget tables (a few columns that already fit a narrow screen) where
   * collapsing to cards adds nothing — they stay a plain table at every
   * width. A `cards` table's cells should carry `label` (see TableCell);
   * a non-`cards` table needs none.
   */
  cards?: boolean;
  /**
   * Collapse to cards up to 1535px instead of the usual 1023px.
   *
   * For a table whose intrinsic width exceeds the content area on a laptop,
   * `lg` is the wrong threshold: it switches to table layout at 1024px and
   * then has nowhere to put the overflow, because the wrapper has no
   * horizontal scroll (see the note above). Opt in per table rather than
   * moving the global breakpoint — most data tables are designed to be wide
   * on a laptop and would lose that.
   */
  wideCards?: boolean;
  /**
   * Let the operator drag column borders to resize them, persisting the
   * widths under this key (`kanade.table.widths.<resizeKey>` in
   * localStorage, per browser). Omit for tables where per-column widths
   * mean nothing — a two-column field/value list, a dashboard widget.
   *
   * Keys must be unique across the app; two tables sharing one would
   * fight over the same widths. Use the page name, plus a suffix when a
   * page has more than one table (`inventory`, `inventory.facts`).
   */
  resizeKey?: string;
  /**
   * Render a [`TableColumnPicker`] just above the table, right-aligned
   * (#1353). Needs `resizeKey`, which is also the picker's storage key.
   *
   * This exists so a page opts in with one word instead of hand-placing
   * the picker: most of these tables sit alone in a ternary branch, where
   * adding a sibling means wrapping the branch in a fragment and
   * re-indenting the whole table. A page that has its own filter row — or
   * wants the picker anywhere else — leaves this off and renders
   * `<TableColumnPicker resizeKey="…" />` itself.
   */
  picker?: boolean;
  /**
   * Offer this table's `agent_meta` attributes as extra columns (#1357).
   * Needs `resizeKey` and `pcIds`, and only makes sense on a table whose
   * rows are about a PC — each `<TableRow>` must pass its `pcId`.
   *
   * Nothing is shown until the operator ticks a key in the picker: the
   * keys are defined per deployment, so there is no default worth having.
   */
  metaColumns?: boolean;
}

export const Table = forwardRef<HTMLTableElement, TableProps>(
  (
    {
      className,
      cards = true,
      wideCards = false,
      resizeKey,
      picker = false,
      metaColumns = false,
      style,
      children,
      ...props
    },
    ref,
  ) => {
    const innerRef = useRef<HTMLTableElement | null>(null);
    const setRefs = useMergedRef(innerRef, ref);

    // Widths come from the shared store (see above) so the reset controls
    // in the page chrome see the same value and can clear it. Persistence
    // rides the store's setter, not an effect here.
    const widths = widthPref.use(resizeKey);
    const setWidths = useCallback(
      (next: ColumnWidths | ((prev: ColumnWidths) => ColumnWidths)) => {
        if (resizeKey) widthPref.update(resizeKey, next);
      },
      [resizeKey],
    );
    // #1353: columns the operator has hidden. Applied as CSS rather than by
    // not rendering the cells, so a page opts in with one line and needs no
    // per-column plumbing of its own.
    const hidden = hiddenPref.use(resizeKey);
    const order = orderPref.use(resizeKey);
    // #1357: metadata columns. Only fetched when the table opted in AND
    // the operator has actually chosen a key — a table nobody has added
    // metadata to issues no request at all.
    const selectedMeta = metaPref.use(resizeKey);
    const metaKeys = metaColumns ? [...selectedMeta] : [];
    const { pcIds, registerPc } = useRegisteredPcIds(metaColumns);
    const metaByPc = useAgentMeta(metaKeys.length ? pcIds : undefined);
    const [layout, setLayout] = useState<{ order: string[]; total: number } | null>(null);
    const [hiddenPositions, setHiddenPositions] = useState<number[]>([]);
    // Scopes the generated stylesheet to THIS table. `useId` produces
    // colons/guillemets that a CSS selector can't carry, so strip them.
    const scopeClass = `kn-cols-${useId().replace(/[^a-zA-Z0-9_-]/g, '')}`;
    const [dragging, setDragging] = useState(false);
    const dragRef = useRef<{ colId: string; startX: number; startWidth: number } | null>(null);

    // The exact breakpoint this table becomes a real table at — the same
    // two thresholds index.css uses. A `cards={false}` table never
    // collapses, so it is always in table mode.
    const inTableMode = useMediaQuery(wideCards ? '(min-width: 1536px)' : '(min-width: 1024px)');
    const active = !!resizeKey && (!cards || inTableMode);

    // Publish this table's columns for a picker rendered outside it, and
    // work out which DOM positions the hidden ones occupy so they can be
    // hidden by stylesheet. Ungated by `active`: hiding a column is
    // meaningful at every width, including card mode where the table isn't
    // a table (there the hidden cell's label:value line disappears).
    //
    // Runs after every render for the same reason the width reconciliation
    // does — the column set belongs to the page, so there is no prop to
    // depend on. Only property reads, no layout flush, and state is only
    // touched when the derived value actually changed.
    useLayoutEffect(() => {
      const table = innerRef.current;
      if (!resizeKey || !table) return;
      const headers = headerCells(table);
      if (!headers.length) return;
      publishColumns(
        resizeKey,
        headers.map((th) => ({ id: columnIdOf(th), label: th.textContent?.trim() ?? '' })),
      );
      // `nth-child` counts every element child regardless of `display`, so
      // these positions stay correct even as other columns are hidden.
      const positions = headers
        .map((th, i) => (hidden.includes(columnIdOf(th)) ? i + 1 : 0))
        .filter((n) => n > 0);
      setHiddenPositions((prev) =>
        prev.length === positions.length && prev.every((n, i) => n === positions[i]) ? prev : positions,
      );
    });

    // Reconcile the stored widths against the columns actually on screen.
    // Deliberately runs after *every* render and reads the DOM: the column
    // set is owned by the page (a column picker, a manifest), so there is
    // no prop here to depend on. Cheap (a dozen header cells) and it only
    // touches state when the derived value really changed, so it can't
    // loop.
    useLayoutEffect(() => {
      const table = innerRef.current;
      if (!active || !table || !Object.keys(widths).length) {
        setLayout((prev) => (prev === null ? prev : null));
        return;
      }
      // Hidden columns are excluded throughout. A `display: none` cell
      // generates no box, so the browser does not count it as a column at
      // all — a <colgroup> that still had an entry for it would apply every
      // subsequent width to the wrong column. Measuring one is equally
      // pointless: it reads 0, which would be stored and then re-applied as
      // a zero-width column the moment the operator showed it again.
      const headers = headerCells(table).filter((th) => !hidden.includes(columnIdOf(th)));
      if (!headers.length) return;
      const order = headers.map(columnIdOf);
      // Every column without a stored width — the rest of the table on the
      // very first resize, or a column the operator has just re-enabled.
      // Under fixed layout an unsized column renders at zero, so measure it
      // where it currently sits and fold it in. This render still shows the
      // old layout, the state update brings the next one in line, so the
      // switch from automatic to fixed layout moves nothing.
      //
      // Measurements are stored as-is, NOT clamped to MIN_COL_WIDTH: that
      // floor is for what a *drag* may do, and applying it here would visibly
      // widen a legitimately narrow column (a badge, an icon button) the
      // first time any other column was resized.
      const missing = order.filter((id) => widths[id] === undefined);
      if (missing.length) {
        setWidths((prev) => {
          const next = { ...prev };
          headers.forEach((th, i) => {
            if (next[order[i]] !== undefined) return;
            // A column that appears while the table is ALREADY in fixed
            // layout measures 0: it has no <col> yet, and fixed layout
            // leaves nothing for an unsized column when the sized ones
            // already account for the whole table. Storing that 0 would
            // freeze it collapsed forever. Only a measurement taken under
            // automatic layout is real, so fall back to a default width.
            const measured = Math.round(th.getBoundingClientRect().width);
            next[order[i]] = measured > 0 ? measured : DEFAULT_NEW_COL_WIDTH;
          });
          return next;
        });
        return;
      }
      const total = order.reduce((sum, id) => sum + widths[id], 0);
      setLayout((prev) =>
        prev && prev.total === total && prev.order.length === order.length && prev.order.every((id, i) => id === order[i])
          ? prev
          : { order, total },
      );
    });

    // Both entry points only ever write the ONE column being changed. The
    // other columns are filled in by the reconciliation effect above, which
    // has to exist anyway (for a column re-added later) and measures them
    // while the table is still in automatic layout — i.e. exactly where the
    // operator sees them. A separate "snapshot everything on pointerdown"
    // pass would duplicate that, and would also freeze the table into fixed
    // layout on a stray click that never moved.
    // Forget this table's columns when it goes away. Without this the entry
    // outlives the table, and a picker mounted for the same key before the
    // table has rendered — a standalone `TableColumnPicker`, or a route that
    // renders one first — would list the previous mount's columns.
    useEffect(() => {
      if (!resizeKey) return;
      return () => publishColumns(resizeKey, []);
    }, [resizeKey]);

    const startResize = useCallback((th: HTMLTableCellElement, clientX: number) => {
      dragRef.current = {
        colId: columnIdOf(th),
        startX: clientX,
        startWidth: Math.round(th.getBoundingClientRect().width),
      };
      setDragging(true);
    }, []);

    const nudge = useCallback((th: HTMLTableCellElement, dx: number) => {
      const id = columnIdOf(th);
      setWidths((prev) => {
        const current = prev[id] ?? Math.round(th.getBoundingClientRect().width);
        return { ...prev, [id]: Math.max(MIN_COL_WIDTH, current + dx) };
      });
    }, [setWidths]);

    const reset = useCallback(() => setWidths({}), [setWidths]);

    // Drag listeners live on the window, not the handle: the pointer
    // routinely leaves the 8px handle mid-drag, and a fast flick can
    // outrun it entirely.
    useEffect(() => {
      if (!dragging) return;
      const onMove = (e: PointerEvent) => {
        const drag = dragRef.current;
        if (!drag) return;
        const next = Math.max(MIN_COL_WIDTH, drag.startWidth + (e.clientX - drag.startX));
        setWidths((prev) => (prev[drag.colId] === next ? prev : { ...prev, [drag.colId]: next }));
      };
      const onEnd = () => {
        dragRef.current = null;
        setDragging(false);
      };
      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onEnd);
      window.addEventListener('pointercancel', onEnd);
      // Keep the resize cursor (and kill text selection) across the whole
      // page while dragging — the pointer is regularly outside the handle.
      document.body.classList.add('kn-col-resizing');
      return () => {
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onEnd);
        window.removeEventListener('pointercancel', onEnd);
        document.body.classList.remove('kn-col-resizing');
      };
    }, [dragging, setWidths]);

    // Reordering is computed during render, not in an effect: an effect
    // would paint the source order for one frame before correcting it,
    // which is exactly the flash a stored preference exists to avoid.
    const sourceIds = resizeKey
      ? (() => {
          const own = sourceColumnIds(children);
          // Metadata columns sit after the page's own, in the order the
          // operator picked them. They carry real ids (`meta:<key>`), so
          // the order / hide / width machinery treats them as ordinary
          // columns from here on.
          return own.length ? [...own, ...metaKeys.map((k) => `meta:${k}`)] : own;
        })()
      : [];
    const perm = permutationFor(sourceIds, order);
    const ctx: ResizeContextValue = {
      widths,
      layout,
      startResize,
      nudge,
      reset,
      active,
      perm,
      sourceIds,
      metaKeys,
      metaByPc,
      registerPc,
    };
    const sized = active && layout !== null;

    // `picker` adds a row ABOVE the card. Only then is there an extra
    // wrapper — a table that didn't ask for one renders exactly the DOM it
    // always did.
    const withPicker = (table: ReactNode) =>
      picker && resizeKey ? (
        <div className="space-y-1.5">
          <div className="flex justify-end">
            <TableColumnPicker resizeKey={resizeKey} metaColumns={metaColumns} />
          </div>
          {table}
        </div>
      ) : (
        table
      );

    return withPicker(
      // No `overflow-*` on the wrapper. An `overflow-x-auto` here used to
      // guard narrow viewports + intrinsically wide rows, but any
      // non-`visible` overflow turns the wrapper into a scroll container
      // (CSS computes the other axis to `auto` too), which pins a
      // `position: sticky` <thead> to *this* box instead of the viewport —
      // so the sticky header would never actually stick as the page scrolls.
      // We keep the whole page as the single scroll context (no horizontal
      // bar, no nested scroll). At `lg`+ the table renders normally with a
      // sticky header; below `lg` the `kn-table` hook drives the row→card
      // transform in index.css so a dense table never overflows a narrow
      // screen. `kn-table` must stay the *direct* parent of <table> — the
      // card CSS selects `.kn-table > table`.
      // `cards` tables get the `.kn-table` card hook (and no overflow clip, so
      // the sticky <thead> resolves against the viewport). A `cards={false}`
      // table stays tabular at every width, so it keeps the classic
      // `overflow-x-auto` safety net — otherwise wide content (long paths, a
      // nested table) would burst out and scroll the whole page sideways.
      <div
        className={cn(
          cards ? 'kn-table' : 'overflow-x-auto',
          cards && wideCards && 'kn-table-wide',
          'rounded-lg border border-border bg-card',
          hiddenPositions.length > 0 && scopeClass,
        )}
        // Once columns are sized the card is exactly as wide as the table
        // it draws a border around — in BOTH directions. Widened past the
        // container it grows with the table rather than letting it render
        // outside the border, and the page takes the horizontal scroll
        // (see the header note above). Narrowed, it shrinks with it.
        //
        // This used to carry `min-width: 100%`, which only looked right
        // while widening: narrowing the columns left the border stretched
        // to the full width with dead space inside it, to the right of the
        // last column. A border that doesn't end where its content does
        // reads as a broken layout rather than as a deliberately narrow
        // table.
        //
        // `cards` only. A `cards={false}` wrapper is already a horizontal
        // scroll container, and growing it to `max-content` would defeat
        // that: the wrapper would widen past its own parent instead of
        // scrolling, which is the one thing its `overflow-x-auto` is there
        // to prevent. Such a table just lets the widened <table> scroll
        // inside it.
        style={sized && cards ? { width: 'max-content' } : undefined}
      >
        {/* Hidden columns (#1353). CSS rather than "don't render the cell":
            a cell's column is decided by the page, in two places (its
            <TableHead> and every row's <TableCell>), so making visibility a
            render-time decision would mean threading a predicate through
            every one of them — 119 header cells and 128 body cells across
            the SPA. A positional rule needs neither.

            `tr:not(:has(> [colspan]))` is load-bearing. An empty-state row
            ("no results") is a single `colSpan` cell, so it is
            `:nth-child(1)`: without the guard, hiding the first column
            would delete the message instead. Rows whose cell count doesn't
            match the header are left entirely alone; their `colSpan` may
            then exceed the column count, which browsers clamp. */}
        {hiddenPositions.length > 0 && (
          <style>
            {hiddenPositions
              .map(
                (n) =>
                  // `!important` because index.css out-specifies this. Its
                  // 1024-1535px block hands ordinary tables back from card
                  // mode with `.kn-table:not(.kn-table-wide) > table > tbody >
                  // tr > td[data-label] { display: table-cell }` — four type
                  // selectors to this rule's two, so without it the header
                  // cell hides and the body cells stay put. "Hidden" has to
                  // beat every layout rule, not tie with them.
                  `.${scopeClass} > table > * > tr:not(:has(> [colspan])) > :nth-child(${n}){display:none!important}`,
              )
              .join('')}
          </style>
        )}
        <table
          ref={setRefs}
          className={cn('w-full text-sm', className)}
          style={sized ? { ...style, tableLayout: 'fixed', width: layout.total } : style}
          {...props}
        >
          {/* Widths live on <col>, not on each <th>: under fixed layout the
              column sizes are what matters, and one colgroup keeps the page
              components free of width plumbing. Only rendered once the
              operator has actually resized something. */}
          {sized && (
            <colgroup>
              {layout.order.map((id, i) => (
                <col key={`${id}-${i}`} style={{ width: widths[id] }} />
              ))}
            </colgroup>
          )}
          <ResizeContext.Provider value={ctx}>{children}</ResizeContext.Provider>
        </table>
      </div>,
    );
  },
);
Table.displayName = 'Table';

interface TableHeaderProps extends HTMLAttributes<HTMLTableSectionElement> {
  /**
   * Pin the header to the top of the viewport as the page scrolls so the
   * column labels stay visible on long lists. Default `true`. Pass
   * `false` for small, self-contained widget tables (dashboard summaries,
   * a handful of rows) where a sticky header adds nothing and can look odd
   * when the table sits inside its own card rather than a full-page list.
   */
  stickyHeader?: boolean;
}

/** True inside <TableHeader>. The header row is the one that stamps each
 *  cell with its source id; body rows only follow the permutation. */
const HeaderContext = createContext(false);

export const TableHeader = forwardRef<HTMLTableSectionElement, TableHeaderProps>(
  ({ className, stickyHeader = true, ...props }, ref) => (
    // Sticky header (table mode, `lg`+ only): as the page scrolls, the
    // column labels stay pinned so the operator never loses track of what
    // each column is. `top-0` — at `lg`+ there is no fixed chrome above
    // the content (the mobile hamburger bar is `md:hidden`). Below `lg`
    // the whole table becomes cards (index.css) and this <thead> is hidden,
    // so the sticky rules are deliberately `lg:`-gated. Needs an *opaque*
    // background so scrolled rows don't bleed through — `bg-card` is the
    // base, with the old faint `bg-muted/5` tint layered on the row.
    <HeaderContext.Provider value={true}>
      <thead
        ref={ref}
        className={cn(
          stickyHeader && 'lg:sticky lg:top-0 lg:z-20 lg:bg-card',
          '[&>tr]:bg-muted/5 text-xs uppercase tracking-wide text-muted',
          className,
        )}
        {...props}
      />
    </HeaderContext.Provider>
  ),
);
TableHeader.displayName = 'TableHeader';

export const TableBody = forwardRef<HTMLTableSectionElement, HTMLAttributes<HTMLTableSectionElement>>(
  ({ className, ...props }, ref) => (
    <tbody ref={ref} className={cn('[&_tr:last-child]:border-0', className)} {...props} />
  ),
);
TableBody.displayName = 'TableBody';

interface TableRowProps extends HTMLAttributes<HTMLTableRowElement> {
  /**
   * The pc_id this row is about (#1357). Only needed on a table using
   * `metaColumns`: it is how the shared row finds this PC's `agent_meta`
   * values. Rows that aren't about a PC — an empty state, a detail row —
   * leave it off and get no metadata cells.
   */
  pcId?: string;
}

export const TableRow = forwardRef<HTMLTableRowElement, TableRowProps>(
  ({ className, children, pcId, ...props }, ref) => {
    const table = useContext(ResizeContext);
    const isHeader = useContext(HeaderContext);
    // Announce this row's PC so the table can fetch its metadata. Depends
    // on `registerPc` itself, not the context object, which is rebuilt
    // every render.
    const registerPc = table?.registerPc;
    useEffect(() => {
      if (!pcId || !registerPc) return;
      return registerPc(pcId);
    }, [pcId, registerPc]);
    return (
      <tr
        ref={ref}
        className={cn('border-b border-border transition-colors hover:bg-muted/5', className)}
        {...props}
      >
        {orderCells(children, table, isHeader, pcId)}
      </tr>
    );
  },
);
TableRow.displayName = 'TableRow';

interface TableHeadProps extends ThHTMLAttributes<HTMLTableCellElement> {
  /**
   * Stable identity for this column's stored width (and, on pages that
   * support it, its position). Required on any table whose column *set*
   * varies at runtime — a column picker, manifest-driven columns — because
   * the fallback identity is the cell's position, which shifts the moment
   * a column is hidden and would hand one column's width to its neighbour.
   * Tables with a fixed column list can omit it.
   */
  colId?: string;
  /**
   * Set `false` to leave this column out of resizing: it gets no handle,
   * so it can only be resized indirectly, by dragging its neighbour.
   *
   * It is NOT exempt from the fixed layout — once any column on the table
   * is resized, this one is frozen at whatever width it had at that moment,
   * exactly like every other column the operator didn't touch. Nothing in
   * this design leaves a single column on automatic width; see §3 of the
   * header comment. Only meaningful on a table with `resizeKey`.
   */
  resizable?: boolean;
}

export const TableHead = forwardRef<HTMLTableCellElement, TableHeadProps>(
  ({ className, colId, resizable = true, children, ...props }, ref) => {
    const { t } = useTranslation('common');
    const resize = useContext(ResizeContext);
    const thRef = useRef<HTMLTableCellElement | null>(null);
    const setRefs = useMergedRef(thRef, ref);
    const showHandle = !!resize?.active && resizable;

    const onPointerDown = (e: ReactPointerEvent<HTMLSpanElement>) => {
      if (e.button !== 0 || !thRef.current) return;
      // Don't let the press start a text selection, and don't let it reach
      // the sort button this header usually wraps.
      e.preventDefault();
      e.stopPropagation();
      resize?.startResize(thRef.current, e.clientX);
    };

    return (
      <th
        ref={setRefs}
        data-col-id={colId}
        className={cn('group relative h-9 px-3 text-left align-middle font-semibold', className)}
        {...props}
      >
        {children}
        {showHandle && (
          <span
            role="separator"
            aria-orientation="vertical"
            aria-label={t('table.resize.label')}
            title={t('table.resize.hint')}
            tabIndex={0}
            onPointerDown={onPointerDown}
            // A click that reached the header would toggle its sort.
            onClick={(e) => e.stopPropagation()}
            onDoubleClick={(e) => {
              e.stopPropagation();
              resize?.reset();
            }}
            onKeyDown={(e) => {
              if (!thRef.current) return;
              const step = e.shiftKey ? NUDGE_STEP_COARSE : NUDGE_STEP;
              if (e.key === 'ArrowLeft') resize?.nudge(thRef.current, -step);
              else if (e.key === 'ArrowRight') resize?.nudge(thRef.current, step);
              else if (e.key === 'Home' || e.key === 'Escape') resize?.reset();
              else return;
              e.preventDefault();
              e.stopPropagation();
            }}
            // `touch-none` stops the browser claiming the gesture for
            // scrolling before pointermove ever fires. The grip itself is
            // the `after:` hairline — the 8px box around it is the target,
            // sized for the pointer rather than the eye.
            className={cn(
              'absolute inset-y-0 right-0 z-10 w-2 cursor-col-resize touch-none select-none',
              'opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 focus-visible:outline-none',
              'after:absolute after:inset-y-1.5 after:right-[3px] after:w-px after:bg-fg/40 after:content-[""]',
              'hover:after:bg-fg focus-visible:after:bg-fg focus-visible:after:inset-y-0.5',
            )}
          />
        )}
      </th>
    );
  },
);
TableHead.displayName = 'TableHead';

interface TableCellProps extends TdHTMLAttributes<HTMLTableCellElement> {
  /**
   * Column name for this cell. Below `lg` the table collapses into cards
   * (index.css) and this text is shown as the cell's label next to its
   * value via `data-label`. Pass the same text as the column's
   * <TableHead>. Omit for columns that need no label in card mode (e.g. an
   * actions-button column) — those cells span the full card width.
   */
  label?: string;
}

/**
 * The column picker for one table (#1353). Drop it in the page's filter
 * row — `<TableColumnPicker resizeKey="events" />` — and that is the whole
 * integration: the columns and their names come from the table's own live
 * header, so there is no list to declare here and no strings to translate
 * twice.
 *
 * Renders nothing until the table has mounted and published a header, and
 * nothing at all for a single-column table. `children` are appended as an
 * extra section, for a page that has its own column-ish choices to offer
 * (Agents picks which `agent_meta` keys become columns at all).
 */
export function TableColumnPicker({
  resizeKey,
  metaColumns = false,
  children,
}: {
  resizeKey: string;
  /** Show the `agent_meta` section (#1357). Set by <Table metaColumns>. */
  metaColumns?: boolean;
  children?: ReactNode;
}) {
  const { t } = useTranslation('common');
  const { columns, toggle, reset, hasHidden, move, resetOrder, hasOrder } =
    useTableColumns(resizeKey);
  const { hasWidths, reset: resetWidths } = useTableWidths(resizeKey);
  const meta = useTableMetaColumns(resizeKey, metaColumns);
  // Every displayed column is listed once, metadata included, so the
  // reorder arrows work uniformly. A metadata column's checkbox removes
  // the key rather than hiding the column: for a column the operator added
  // themselves those are the same intent, and having both a "hide" here
  // and a "remove" below would be two controls disagreeing about one
  // column. The metadata section is therefore purely an ADD menu — it
  // lists only the keys not already shown.
  const unaddedMetaKeys = meta.available.filter((k) => !meta.selected.includes(k));
  const toggleColumn = (id: string) =>
    id.startsWith('meta:') ? meta.toggle(id.slice('meta:'.length)) : toggle(id);
  const visibleCount = columns.filter((c) => c.visible).length;
  if (columns.length < 2 && !children) return null;

  return (
    <details className="relative">
      <summary className="flex h-8 cursor-pointer list-none items-center gap-1.5 rounded-md border border-border px-2.5 text-sm hover:bg-muted/10">
        <SlidersHorizontal className="size-3.5" />
        {t('table.columns.pick')}
      </summary>
      {/* z-50, not z-20: the table's sticky <thead> is `lg:z-20`. At equal
          z-index the later DOM node wins, and the table comes after this
          filter row — so the header row would paint straight through the
          open panel once the page is scrolled. */}
      <div className="absolute right-0 z-50 mt-1 max-h-72 w-56 overflow-auto rounded-md border border-border bg-card p-2 shadow-lg">
        {columns.map((c, i) => (
          <label
            key={c.id}
            className={cn(
              'flex items-center gap-2 rounded px-1 py-0.5 text-sm hover:bg-muted/10',
              // The last visible column can't be unchecked — a table with
              // no columns has no way back, since the picker would have
              // nothing left to list.
              c.visible && visibleCount <= 1 ? 'cursor-not-allowed opacity-60' : 'cursor-pointer',
            )}
          >
            <input
              type="checkbox"
              checked={c.visible}
              disabled={c.visible && visibleCount <= 1}
              onChange={() => toggleColumn(c.id)}
            />
            <span className="flex-1 truncate">{c.label || t('table.columns.unnamed', { n: i + 1 })}</span>
            {/* Reorder lives here rather than as a drag on the header:
                the header already carries a click (sort) and a drag on its
                right edge (resize), and this keeps working below the card
                breakpoint where there is no header row at all. */}
            <span className="flex shrink-0 items-center">
              <button
                type="button"
                aria-label={t('table.columns.moveUp', { name: c.label || String(i + 1) })}
                disabled={i === 0}
                onClick={(e) => {
                  e.preventDefault();
                  move(c.id, -1);
                }}
                className="rounded p-0.5 text-muted hover:bg-muted/20 hover:text-fg disabled:pointer-events-none disabled:opacity-30"
              >
                <ChevronUp className="size-3.5" />
              </button>
              <button
                type="button"
                aria-label={t('table.columns.moveDown', { name: c.label || String(i + 1) })}
                disabled={i === columns.length - 1}
                onClick={(e) => {
                  e.preventDefault();
                  move(c.id, 1);
                }}
                className="rounded p-0.5 text-muted hover:bg-muted/20 hover:text-fg disabled:pointer-events-none disabled:opacity-30"
              >
                <ChevronDown className="size-3.5" />
              </button>
            </span>
          </label>
        ))}
        {metaColumns && unaddedMetaKeys.length > 0 && (
          <>
            <div className="mt-2 px-1 pb-1 text-[10px] font-medium uppercase tracking-wide text-muted">
              {t('table.columns.metaSection')}
            </div>
            {/* An ADD menu, never pre-ticked: these keys are whatever the
                fleet's administrator decided to record, so there is no
                default this code could pick for anyone. Once added, the
                key moves up into the list above with everything else. */}
            {unaddedMetaKeys.map((key) => (
              <button
                key={key}
                type="button"
                onClick={() => meta.toggle(key)}
                className="flex w-full items-center gap-2 rounded px-1 py-0.5 text-left text-sm text-muted hover:bg-muted/10 hover:text-fg"
              >
                <Plus className="size-3.5 shrink-0" />
                <span className="truncate">{key}</span>
              </button>
            ))}
          </>
        )}
        {children}
        {(hasHidden || hasWidths || hasOrder) && (
          <div className="mt-2 space-y-0.5 border-t border-border pt-1.5">
            {hasHidden && (
              <button
                type="button"
                onClick={reset}
                className="flex w-full items-center gap-1.5 rounded px-1 py-0.5 text-left text-xs text-muted hover:bg-muted/10 hover:text-fg"
              >
                <RotateCcw className="size-3" />
                {t('table.columns.showAll')}
              </button>
            )}
            {hasOrder && (
              <button
                type="button"
                onClick={resetOrder}
                className="flex w-full items-center gap-1.5 rounded px-1 py-0.5 text-left text-xs text-muted hover:bg-muted/10 hover:text-fg"
              >
                <RotateCcw className="size-3" />
                {t('table.columns.resetOrder')}
              </button>
            )}
            {hasWidths && (
              <button
                type="button"
                onClick={resetWidths}
                className="flex w-full items-center gap-1.5 rounded px-1 py-0.5 text-left text-xs text-muted hover:bg-muted/10 hover:text-fg"
              >
                <RotateCcw className="size-3" />
                {t('table.resize.reset')}
              </button>
            )}
          </div>
        )}
      </div>
    </details>
  );
}

export const TableCell = forwardRef<HTMLTableCellElement, TableCellProps>(
  ({ className, label, ...props }, ref) => (
    <td ref={ref} data-label={label} className={cn('px-3 py-2 align-middle', className)} {...props} />
  ),
);
TableCell.displayName = 'TableCell';
