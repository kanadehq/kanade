import { forwardRef, type HTMLAttributes, type TdHTMLAttributes, type ThHTMLAttributes } from 'react';

import { cn } from '@/lib/utils';

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
}

export const Table = forwardRef<HTMLTableElement, TableProps>(
  ({ className, cards = true, ...props }, ref) => (
    // No `overflow-*` on the wrapper. An `overflow-x-auto` here used to
    // guard narrow viewports + intrinsically wide rows, but any
    // non-`visible` overflow turns the wrapper into a scroll container
    // (CSS computes the other axis to `auto` too), which pins a
    // `position: sticky` <thead> to *this* box instead of the viewport —
    // so the sticky header would never actually stick as the page scrolls.
    // We keep the whole page as the single scroll context (no horizontal
    // bar, no nested scroll). At `lg`+ the table renders normally with a
    // sticky header; below `lg` the `.kn-table` hook drives the row→card
    // transform in index.css so a dense table never overflows a narrow
    // screen. `kn-table` must stay the *direct* parent of <table> — the
    // card CSS selects `.kn-table > table`.
    // `cards` tables get the `.kn-table` card hook (and no overflow clip, so
    // the sticky <thead> resolves against the viewport). A `cards={false}`
    // table stays tabular at every width, so it keeps the classic
    // `overflow-x-auto` safety net — otherwise wide content (long paths, a
    // nested table) would burst out and scroll the whole page sideways.
    <div className={cn(cards ? 'kn-table' : 'overflow-x-auto', 'rounded-lg border border-border bg-card')}>
      <table ref={ref} className={cn('w-full text-sm', className)} {...props} />
    </div>
  ),
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

export const TableHead = forwardRef<HTMLTableCellElement, ThHTMLAttributes<HTMLTableCellElement>>(
  ({ className, ...props }, ref) => (
    <th ref={ref} className={cn('h-9 px-3 text-left align-middle font-semibold', className)} {...props} />
  ),
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
