import { describe, expect, test } from 'bun:test';

import {
  filterColumnsOf,
  filterToParam,
  opsForColumn,
  parseFilterTokens,
  PC_ID_FILTER_COLUMN,
} from './Search';

// pc_id is not a manifest column, but operators need to filter by it on
// every tab. It is offered as a built-in text column in the filter UI.

describe('filterColumnsOf', () => {
  test('appends pc_id last so the default filter column is unchanged', () => {
    const cols = filterColumnsOf([{ field: 'name', type: 'text' }]);
    expect(cols.map((c) => c.field)).toEqual(['name', 'pc_id']);
  });

  test('offers only pc_id when the tab has no columns', () => {
    expect(filterColumnsOf([])).toEqual([PC_ID_FILTER_COLUMN]);
  });

  test('does not duplicate a manifest column named pc_id', () => {
    const cols = filterColumnsOf([
      { field: 'pc_id', type: 'integer' },
      { field: 'name', type: 'text' },
    ]);
    expect(cols.filter((c) => c.field === 'pc_id')).toHaveLength(1);
    expect(cols[cols.length - 1]).toEqual(PC_ID_FILTER_COLUMN);
  });
});

describe('pc_id filter', () => {
  test('uses the text operator set', () => {
    expect(opsForColumn(PC_ID_FILTER_COLUMN)).toEqual(['eq', 'contains', 'prefix', 'ne']);
  });

  test('builds backend query params', () => {
    const f = { uid: 1, column: 'pc_id', value: 'PC-01' };
    expect(filterToParam({ ...f, op: 'eq' })).toEqual(['pc_id', 'PC-01']);
    expect(filterToParam({ ...f, op: 'contains' })).toEqual(['pc_id__contains', 'PC-01']);
  });

  test('round-trips through the shareable URL token, dots in the value included', () => {
    let n = 0;
    const got = parseFilterTokens(['pc_id.contains.PC-01.corp'], () => ++n);
    expect(got).toEqual([{ uid: 1, column: 'pc_id', op: 'contains', value: 'PC-01.corp' }]);
  });
});
