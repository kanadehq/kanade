import {
  createContext,
  forwardRef,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type ForwardedRef,
  type HTMLAttributes,
  type MutableRefObject,
  type PointerEvent as ReactPointerEvent,
  type TdHTMLAttributes,
  type ThHTMLAttributes,
} from 'react';
import { useTranslation } from 'react-i18next';

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
const WIDTHS_PREFIX = 'kanade.table.widths.';

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

/** A column's identity for width storage: the explicit `colId` a page
 *  passed to <TableHead>, else the cell's position. Pages whose column
 *  *set* varies (a column picker, manifest-driven columns) must pass
 *  `colId` — position shifts when a column is hidden, which would hand
 *  one column's stored width to its neighbour. */
function columnIdOf(th: HTMLTableCellElement): string {
  return th.dataset.colId ?? String(th.cellIndex);
}

/** The header cells that define the columns. Fixed layout sizes a table
 *  from its first row, and that is the row the handles live in. */
function headerCells(table: HTMLTableElement): HTMLTableCellElement[] {
  const row = table.tHead?.rows[0];
  return row ? (Array.from(row.cells) as HTMLTableCellElement[]) : [];
}

function readWidths(key: string): ColumnWidths {
  try {
    const raw = localStorage.getItem(WIDTHS_PREFIX + key);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
    const out: ColumnWidths = {};
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      // Drop anything that isn't a usable width — a corrupted or
      // hand-edited entry would otherwise reach `<col width>` as NaN and
      // collapse the column. Not floored at MIN_COL_WIDTH: measured widths
      // (see the reconciliation effect) can legitimately be narrower, and
      // discarding one here would make the column look "missing" and get
      // re-measured under a layout it no longer matches.
      if (typeof v === 'number' && Number.isFinite(v) && v > 0) out[k] = v;
    }
    return out;
  } catch {
    /* blocked storage / malformed JSON — fall back to automatic layout */
    return {};
  }
}

function writeWidths(key: string, widths: ColumnWidths) {
  try {
    if (Object.keys(widths).length) localStorage.setItem(WIDTHS_PREFIX + key, JSON.stringify(widths));
    // Nothing stored ⇒ remove the key rather than leaving `{}` behind, so
    // "has this table been resized?" is a plain key-exists question.
    else localStorage.removeItem(WIDTHS_PREFIX + key);
  } catch {
    /* non-persistent this session; not worth surfacing */
  }
}

/* ------------------------------------------------------------------ *
 * The widths live in a module-level store rather than in <Table>'s own
 * state, because the reset control is not inside the table: the Agents
 * column picker and the Settings page both need to see whether a table
 * has stored widths and to clear them, and a table's own `useState` is
 * not reachable from there. One store per `resizeKey`, subscribed to via
 * `useSyncExternalStore` so every reader re-renders on a change.
 * ------------------------------------------------------------------ */

type WidthStore = { widths: ColumnWidths; listeners: Set<() => void> };
const widthStores = new Map<string, WidthStore>();
/** Stable empty snapshot — `useSyncExternalStore` compares by identity, so
 *  returning a fresh `{}` per call would loop forever. */
const NO_WIDTHS: ColumnWidths = Object.freeze({});

function widthStore(key: string): WidthStore {
  let store = widthStores.get(key);
  if (!store) {
    store = { widths: readWidths(key), listeners: new Set() };
    widthStores.set(key, store);
  }
  return store;
}

function updateWidths(key: string, next: ColumnWidths | ((prev: ColumnWidths) => ColumnWidths)) {
  const store = widthStore(key);
  const value = typeof next === 'function' ? next(store.widths) : next;
  if (value === store.widths) return;
  store.widths = value;
  writeWidths(key, value);
  for (const notify of store.listeners) notify();
}

function useWidths(key: string | undefined): ColumnWidths {
  const subscribe = useCallback(
    (onChange: () => void) => {
      if (!key) return () => {};
      const store = widthStore(key);
      store.listeners.add(onChange);
      return () => store.listeners.delete(onChange);
    },
    [key],
  );
  const snapshot = useCallback(() => (key ? widthStore(key).widths : NO_WIDTHS), [key]);
  return useSyncExternalStore(subscribe, snapshot);
}

/**
 * Read/clear one table's stored column widths from outside the table —
 * for a "reset widths" control that lives in the page's own chrome.
 * `hasWidths` is false until the operator resizes something, which is the
 * cue to render no control at all rather than a dead one.
 */
export function useTableWidths(resizeKey: string): { hasWidths: boolean; reset: () => void } {
  const widths = useWidths(resizeKey);
  const reset = useCallback(() => updateWidths(resizeKey, {}), [resizeKey]);
  return { hasWidths: Object.keys(widths).length > 0, reset };
}

/**
 * Clear the stored widths of EVERY table, including tables not currently
 * mounted (their entry is only in localStorage). The global escape hatch
 * for "I made some table unreadable and can't remember which page".
 * Returns how many tables were reset.
 */
export function resetAllTableWidths(): number {
  const keys = new Set<string>(widthStores.keys());
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const raw = localStorage.key(i);
      if (raw?.startsWith(WIDTHS_PREFIX)) keys.add(raw.slice(WIDTHS_PREFIX.length));
    }
  } catch {
    /* storage unreadable — fall back to the mounted tables alone */
  }
  let cleared = 0;
  for (const key of keys) {
    // Count only tables that actually had something stored, so the caller
    // can report a truthful number.
    if (Object.keys(widthStore(key).widths).length) cleared++;
    updateWidths(key, {});
  }
  return cleared;
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
}

export const Table = forwardRef<HTMLTableElement, TableProps>(
  ({ className, cards = true, wideCards = false, resizeKey, style, children, ...props }, ref) => {
    const innerRef = useRef<HTMLTableElement | null>(null);
    const setRefs = useMergedRef(innerRef, ref);

    // Widths come from the shared store (see above) so the reset controls
    // in the page chrome see the same value and can clear it. Persistence
    // rides the store's setter, not an effect here.
    const widths = useWidths(resizeKey);
    const setWidths = useCallback(
      (next: ColumnWidths | ((prev: ColumnWidths) => ColumnWidths)) => {
        if (resizeKey) updateWidths(resizeKey, next);
      },
      [resizeKey],
    );
    const [layout, setLayout] = useState<{ order: string[]; total: number } | null>(null);
    const [dragging, setDragging] = useState(false);
    const dragRef = useRef<{ colId: string; startX: number; startWidth: number } | null>(null);

    // The exact breakpoint this table becomes a real table at — the same
    // two thresholds index.css uses. A `cards={false}` table never
    // collapses, so it is always in table mode.
    const inTableMode = useMediaQuery(wideCards ? '(min-width: 1536px)' : '(min-width: 1024px)');
    const active = !!resizeKey && (!cards || inTableMode);

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
      const headers = headerCells(table);
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
            if (next[order[i]] === undefined) next[order[i]] = Math.round(th.getBoundingClientRect().width);
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

    const ctx: ResizeContextValue = { widths, layout, startResize, nudge, reset, active };
    const sized = active && layout !== null;

    return (
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
        )}
        // Resized past the container, the wrapper grows with the table
        // instead of letting it render outside the card border — the page
        // takes the horizontal scroll (see the header note above). Still
        // `min-width: 100%` so a table narrowed below the container keeps
        // filling it.
        //
        // `cards` only. A `cards={false}` wrapper is already a horizontal
        // scroll container, and growing it to `max-content` would defeat
        // that: the wrapper would widen past its own parent instead of
        // scrolling, which is the one thing its `overflow-x-auto` is there
        // to prevent. Such a table just lets the widened <table> scroll
        // inside it.
        style={sized && cards ? { width: 'max-content', minWidth: '100%' } : undefined}
      >
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
      </div>
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
    <thead
      ref={ref}
      className={cn(
        stickyHeader && 'lg:sticky lg:top-0 lg:z-20 lg:bg-card',
        '[&>tr]:bg-muted/5 text-xs uppercase tracking-wide text-muted',
        className,
      )}
      {...props}
    />
  ),
);
TableHeader.displayName = 'TableHeader';

export const TableBody = forwardRef<HTMLTableSectionElement, HTMLAttributes<HTMLTableSectionElement>>(
  ({ className, ...props }, ref) => (
    <tbody ref={ref} className={cn('[&_tr:last-child]:border-0', className)} {...props} />
  ),
);
TableBody.displayName = 'TableBody';

export const TableRow = forwardRef<HTMLTableRowElement, HTMLAttributes<HTMLTableRowElement>>(
  ({ className, ...props }, ref) => (
    <tr ref={ref} className={cn('border-b border-border transition-colors hover:bg-muted/5', className)} {...props} />
  ),
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

export const TableCell = forwardRef<HTMLTableCellElement, TableCellProps>(
  ({ className, label, ...props }, ref) => (
    <td ref={ref} data-label={label} className={cn('px-3 py-2 align-middle', className)} {...props} />
  ),
);
TableCell.displayName = 'TableCell';
