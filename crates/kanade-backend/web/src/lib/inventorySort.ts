// Sort keys for the Inventory fleet table's manifest-driven columns.
//
// The shared table sorts on a cell's rendered text unless the cell hands it
// a `sortValue`. For the fleet's `f:<field>` columns the text is a formatted
// size ("900 MiB" vs "1.5 GiB") or a localised date, so text order is wrong;
// this returns the raw comparable instead. `undefined` means "no better key
// than the display text".

export type FleetFieldType = 'number' | 'bytes' | 'timestamp' | 'table';

export function fleetSortValue(val: unknown, type?: FleetFieldType): string | number | undefined {
  if (val === null || val === undefined || val === '') return '';
  switch (type) {
    case 'number':
    case 'bytes': {
      const n = Number(val);
      return Number.isFinite(n) ? n : undefined;
    }
    case 'timestamp': {
      const ms = typeof val === 'string' ? Date.parse(val) : Number.NaN;
      return Number.isNaN(ms) ? '' : ms;
    }
    case 'table':
      return Array.isArray(val) ? val.length : '';
    default:
      return typeof val === 'number' && Number.isFinite(val) ? val : undefined;
  }
}

/** Sort key for an ISO timestamp column; blank when absent or unparsable. */
export function isoSortValue(iso: string | null | undefined): string | number {
  if (!iso) return '';
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? '' : ms;
}
