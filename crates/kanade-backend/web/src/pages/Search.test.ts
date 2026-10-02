import { describe, expect, test } from 'bun:test';

import {
  buildSearchUrl,
  fetchAllSearchRows,
  filterColumnsOf,
  filterToParam,
  opsForColumn,
  parseFilterTokens,
  PC_ID_FILTER_COLUMN,
  searchExportFilename,
  searchRowsToCsv,
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

describe('CSV export helpers', () => {
  const mk = (n: number, start = 0) =>
    Array.from({ length: n }, (_, i) => ({ pc_id: `pc${start + i}`, name: `n${start + i}` }));

  test('buildSearchUrl: explode and scalar endpoints, limit/offset', () => {
    const base = { manifestId: 'sw', field: 'apps', isScalar: false, filters: [], limit: 10, offset: 0 };
    expect(buildSearchUrl(base)).toBe('/api/inventory/sw/search/apps?limit=10');
    expect(buildSearchUrl({ ...base, isScalar: true, offset: 20 })).toBe(
      '/api/inventory/sw/search-scalars?limit=10&offset=20',
    );
  });

  test('searchRowsToCsv: header, empty for null, JSON for objects', () => {
    const out = searchRowsToCsv(
      [{ field: 'a' }, { field: 'b' }],
      [{ pc_id: 'p1', a: null, b: { x: 1 } }, { pc_id: 'p2', a: 3, b: true }],
    );
    expect(out).toEqual([
      ['pc_id', 'a', 'b'],
      ['p1', '', '{"x":1}'],
      ['p2', '3', 'true'],
    ]);
  });

  test('searchExportFilename pads fields', () => {
    expect(searchExportFilename('sw', 'scalar', new Date(2026, 0, 2, 3, 4))).toBe(
      'inventory-search_sw_scalar_20260102_0304.csv',
    );
  });

  test('stops on a short page', async () => {
    const pages = [mk(3), mk(2, 3)];
    const offs: number[] = [];
    const got = await fetchAllSearchRows(async (o) => (offs.push(o), pages[offs.length - 1]), ['name'], 3);
    expect(got.length).toBe(5);
    expect(offs).toEqual([0, 3]);
  });

  test('exact multiple costs one trailing empty page', async () => {
    const pages = [mk(2), mk(2, 2), []];
    let i = 0;
    const got = await fetchAllSearchRows(async () => pages[i++], ['name'], 2);
    expect(got.length).toBe(4);
    expect(i).toBe(3);
  });

  test('dedups boundary overlap without ending early; keeps distinct keys per PC', async () => {
    const pages = [
      [{ pc_id: 'a', name: '1' }, { pc_id: 'a', name: '2' }],
      [{ pc_id: 'a', name: '2' }, { pc_id: 'b', name: '1' }],
      [{ pc_id: 'b', name: '1' }],
    ];
    let i = 0;
    const got = await fetchAllSearchRows(async () => pages[i++], ['name'], 2);
    expect(got.map((r) => `${r.pc_id}${r.name}`)).toEqual(['a1', 'a2', 'b1']);
  });

  test('stops when a full page adds nothing new', async () => {
    let calls = 0;
    const got = await fetchAllSearchRows(async () => (calls++, mk(2)), [], 2);
    expect(got.length).toBe(2);
    expect(calls).toBe(2);
  });

  test('propagates mid-walk failure', async () => {
    let i = 0;
    const p = fetchAllSearchRows(async () => {
      if (i++ === 1) throw new Error('boom');
      return mk(2);
    }, [], 2);
    await expect(p).rejects.toThrow('boom');
  });
});
