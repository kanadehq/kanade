// Mount targets for table.ct.tsx. Playwright CT bundles only what the
// test file *imports*, so a component defined inline in the spec would
// never reach the browser — hence this file.
import { resetAllTableWidths, Table, TableBody, TableCell, TableHead, TableHeader, TableRow, useTableWidths } from './table';

const COLUMNS = ['alpha', 'bravo', 'charlie', 'delta'];

/**
 * A resizable table with four fixed-width-free columns. `hide` drops one
 * column so a test can reproduce what a column picker does to the column
 * set after widths were already stored.
 */
export function ResizableTable({ hide }: { hide?: string }) {
  const cols = COLUMNS.filter((c) => c !== hide);
  return (
    <Table resizeKey="ct">
      <TableHeader>
        <TableRow>
          {cols.map((c) => (
            <TableHead key={c} colId={c}>
              {c}
            </TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        <TableRow>
          {cols.map((c) => (
            <TableCell key={c} label={c}>
              {c} value
            </TableCell>
          ))}
        </TableRow>
      </TableBody>
    </Table>
  );
}

/**
 * The table plus a reset control that lives OUTSIDE it, the way the Agents
 * column picker and the Settings page do. This is what the shared width
 * store exists for: the button has to see the table's widths without being
 * inside it, and must appear only once something has been resized.
 */
export function TableWithExternalReset() {
  const { hasWidths, reset } = useTableWidths('ct');
  return (
    <div>
      {hasWidths && (
        <button type="button" onClick={reset}>
          reset widths
        </button>
      )}
      <button type="button" onClick={() => resetAllTableWidths()}>
        reset all
      </button>
      <ResizableTable />
    </div>
  );
}

/**
 * A resizable table that never collapses into cards. Its wrapper is a
 * horizontal scroll container (`overflow-x-auto`), so a widened table has
 * to scroll INSIDE it rather than grow it — the opposite of what a `cards`
 * table does.
 */
export function NonCardResizableTable() {
  return (
    <Table cards={false} resizeKey="ct-nocards">
      <TableHeader>
        <TableRow>
          {COLUMNS.map((c) => (
            <TableHead key={c} colId={c}>
              {c}
            </TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        <TableRow>
          {COLUMNS.map((c) => (
            <TableCell key={c}>{c} value</TableCell>
          ))}
        </TableRow>
      </TableBody>
    </Table>
  );
}

/** The same table without `resizeKey` — the shape every table that didn't
 *  opt in still has to render. */
export function PlainTable() {
  return (
    <Table>
      <TableHeader>
        <TableRow>
          {COLUMNS.map((c) => (
            <TableHead key={c}>{c}</TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        <TableRow>
          {COLUMNS.map((c) => (
            <TableCell key={c} label={c}>
              {c} value
            </TableCell>
          ))}
        </TableRow>
      </TableBody>
    </Table>
  );
}
