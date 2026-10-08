// Host for the fleet-table component tests. Mounts the real `FleetTable`
// (not a copy) with a recorder for row clicks, so a test can assert which
// PC a click resolved to after the rows were re-ordered.
import { useState } from 'react';

import { FleetTable } from './Inventory';

type Field = { field: string; label: string; type?: 'number' | 'bytes' | 'timestamp' | 'table'; columns?: never };
type Row = {
  pc_id: string;
  facts: Record<string, unknown>;
  collected_at: string | null;
  last_logon_user: string | null;
  last_logon_display_name: string | null;
};

const COLUMNS: Field[] = [
  { field: 'os', label: 'OS' },
  { field: 'ram', label: 'RAM', type: 'bytes' },
  { field: 'disks', label: 'Disks', type: 'table' },
];

const GIB = 1024 ** 3;
const ROWS: Row[] = [
  { pc_id: 'pc-a', facts: { os: 'Win11', ram: 1.5 * GIB, disks: [1, 2] }, collected_at: '2026-03-01T00:00:00Z', last_logon_user: 'alice', last_logon_display_name: null },
  { pc_id: 'pc-b', facts: { os: 'Win10', ram: 900 * 1024 ** 2, disks: [1] }, collected_at: '2026-01-15T00:00:00Z', last_logon_user: null, last_logon_display_name: null },
  { pc_id: 'pc-c', facts: { os: 'Win11', ram: 16 * GIB, disks: [1, 2, 3] }, collected_at: '2026-12-01T00:00:00Z', last_logon_user: 'carol', last_logon_display_name: null },
  { pc_id: 'pc-d', facts: { os: 'Win10' }, collected_at: null, last_logon_user: 'dave', last_logon_display_name: null },
];

export function FleetHarness() {
  const [picked, setPicked] = useState('none');
  return (
    <div className="w-[1100px] bg-background p-4">
      <FleetTable columns={COLUMNS} rows={ROWS} pickPc={setPicked} />
      <output data-testid="picked">{picked}</output>
    </div>
  );
}
