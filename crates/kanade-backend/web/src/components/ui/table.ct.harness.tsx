// Mount targets for table.ct.tsx. Playwright CT bundles only what the
// test file *imports*, so a component defined inline in the spec would
// never reach the browser — hence this file.
import {
  resetAllTableWidths,
  Table,
  TableBody,
  TableCell,
  TableColumnPicker,
  TableHead,
  TableHeader,
  TableRow,
  useTableColumns,
  useTableWidths,
} from './table';

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

/**
 * A resizable table WITH the shared column picker, plus the two row shapes
 * that make hiding tricky: an ordinary row, and an empty-state row that is
 * a single `colSpan` cell. The latter must survive hiding column 1 — it is
 * `:nth-child(1)`, so an unguarded positional rule deletes it.
 */
export function TableWithPicker({ empty = false }: { empty?: boolean }) {
  return (
    <Table resizeKey="ct-pick" picker>
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
        {empty ? (
          <TableRow>
            <TableCell colSpan={COLUMNS.length}>nothing here</TableCell>
          </TableRow>
        ) : (
          <TableRow>
            {COLUMNS.map((c) => (
              <TableCell key={c} label={c}>
                {c} value
              </TableCell>
            ))}
          </TableRow>
        )}
      </TableBody>
    </Table>
  );
}

/** The picker rendered on its own, the way a page with its own filter row
 *  would place it. With no table mounted for the key it has nothing to
 *  list — including after a table for that key has come and gone. */
export function LonePicker({ resizeKey = 'ct-lonely' }: { resizeKey?: string }) {
  return <TableColumnPicker resizeKey={resizeKey} />;
}

/**
 * Calls `useTableColumns().toggle` from plain buttons, with none of the
 * picker's own guards. The picker disables the last visible column's
 * checkbox, which means the *store's* refusal to hide it is unreachable
 * through the UI — this is what exercises that contract directly, since
 * `useTableColumns` is exported for pages to build their own controls.
 */
export function TableWithForcedToggle() {
  const { columns, toggle } = useTableColumns('ct-force');
  return (
    <div>
      {columns.map((c) => (
        <button key={c.id} type="button" onClick={() => toggle(c.id)}>
          toggle {c.id}
        </button>
      ))}
      {/* Two toggles in ONE tick. React batches the re-render, so a guard
          that read a count captured at render time would let both through
          and leave the table with no columns at all. */}
      <button
        type="button"
        onClick={() => {
          toggle('charlie');
          toggle('delta');
        }}
      >
        toggle last two at once
      </button>
      <Table resizeKey="ct-force">
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
              <TableCell key={c} label={c}>
                {c} value
              </TableCell>
            ))}
          </TableRow>
        </TableBody>
      </Table>
    </div>
  );
}
