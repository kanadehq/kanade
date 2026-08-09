// Mount targets for table.ct.tsx. Playwright CT bundles only what the
// test file *imports*, so a component defined inline in the spec would
// never reach the browser — hence this file.
import { useState } from 'react';

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
export function TableWithPicker({ empty = false, withInput = false }: { empty?: boolean; withInput?: boolean }) {
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
          // Two cells, not one: this mirrors the real expanded/empty rows
          // (Jobs, Schedules) and is what makes a missing cell-count guard
          // visible — permuting a 2-cell row against a 4-column header
          // indexes off the end and drops the message.
          <TableRow>
            <TableCell />
            <TableCell colSpan={COLUMNS.length - 1}>nothing here</TableCell>
          </TableRow>
        ) : (
          <TableRow>
            {COLUMNS.map((c) => (
              <TableCell key={c} label={c}>
                {withInput && c === 'charlie' ? <input aria-label="cell input" /> : `${c} value`}
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

/**
 * `useTableColumns().move` driven from plain buttons, with none of the
 * picker's own guards. The picker disables the arrow at each end, which
 * makes the *store's* refusal to move past it unreachable through the UI —
 * and `useTableColumns` is exported for pages to build their own controls.
 */
export function TableWithForcedMove() {
  const { columns, move } = useTableColumns('ct-pick');
  return (
    <div>
      {columns.map((c) => (
        <span key={c.id}>
          <button type="button" onClick={() => move(c.id, -1)}>
            force {c.id} left
          </button>
          <button type="button" onClick={() => move(c.id, 1)}>
            force {c.id} right
          </button>
        </span>
      ))}
      <TableWithPicker />
    </div>
  );
}

/**
 * A table whose headers carry NO `colId` — the shape most pages have — and
 * whose column set can grow, standing in for a release that inserts a
 * column into the page's JSX. Preferences stored against the old shape
 * must not land on the new neighbours.
 */
export function TableWithoutColIds({ inserted = false }: { inserted?: boolean }) {
  const cols = inserted ? ['alpha', 'inserted', 'bravo', 'charlie', 'delta'] : COLUMNS;
  return (
    <Table resizeKey="ct-noid" picker>
      <TableHeader>
        <TableRow>
          {cols.map((c) => (
            <TableHead key={c}>{c}</TableHead>
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

const META_PCS = ['pc-a', 'pc-b'];

/**
 * A table with `metaColumns` on, standing in for the seven pc_id-keyed
 * pages. `pc-b` deliberately has no attributes on the stubbed endpoint —
 * a PC with nothing recorded must render an empty cell, not lose the
 * column, since the column is a fleet-wide choice and not a per-row one.
 */
export function TableWithMetaColumns() {
  return (
    <Table resizeKey="ct-meta" picker metaColumns>
      <TableHeader>
        <TableRow>
          <TableHead colId="pcId">pc_id</TableHead>
          <TableHead colId="os">os</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {META_PCS.map((pc) => (
          <TableRow key={pc} pcId={pc}>
            <TableCell label="pc_id">{pc}</TableCell>
            <TableCell label="os">windows</TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

/**
 * Rows that appear AFTER the table has already fetched metadata, the way
 * Compliance's "ok" rows do — a child component fetches them itself, so
 * the parent never had a list of pc_ids to hand over. The late rows have
 * to pull their own metadata in.
 */
export function TableWithLateRows() {
  const [expanded, setExpanded] = useState(false);
  return (
    <div>
      <button type="button" onClick={() => setExpanded(true)}>
        expand
      </button>
      <Table resizeKey="ct-late" picker metaColumns>
        <TableHeader>
          <TableRow>
            <TableHead colId="pcId">pc_id</TableHead>
            <TableHead colId="os">os</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          <TableRow pcId="pc-a">
            <TableCell label="pc_id">pc-a</TableCell>
            <TableCell label="os">windows</TableCell>
          </TableRow>
          {expanded && (
            <TableRow pcId="pc-late">
              <TableCell label="pc_id">pc-late</TableCell>
              <TableCell label="os">windows</TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>
    </div>
  );
}

/**
 * The same pc_id on two rows, one of which can be removed — the shape
 * Compliance has, where a PC appears once per check. The registration is
 * counted rather than a set precisely so the first row to unmount doesn't
 * take the other's registration (and therefore its values) with it.
 */
export function TableWithDuplicatePc() {
  const [both, setBoth] = useState(true);
  return (
    <div>
      <button type="button" onClick={() => setBoth(false)}>
        drop one
      </button>
      <Table resizeKey="ct-dup" picker metaColumns>
        <TableHeader>
          <TableRow>
            <TableHead colId="pcId">pc_id</TableHead>
            <TableHead colId="check">check</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {both && (
            <TableRow pcId="pc-a">
              <TableCell label="pc_id">pc-a</TableCell>
              <TableCell label="check">bitlocker</TableCell>
            </TableRow>
          )}
          <TableRow pcId="pc-a">
            <TableCell label="pc_id">pc-a</TableCell>
            <TableCell label="check">firewall</TableCell>
          </TableRow>
        </TableBody>
      </Table>
    </div>
  );
}
