import { describe, expect, test } from 'bun:test';
import { encodeTableState, parseTableState, tableUrl } from './inventoryTable';

describe('inventory table query state', () => {
  test('blank, empty text and literal null checklist values remain distinct in shared URLs', () => {
    const state = parseTableState('{"filters":{"f:name":{"values":[null,"","null"]}}}');
    expect(state.filters['f:name'].values).toEqual(['null', '""', '"null"']);
    expect(JSON.parse(encodeTableState(state))).toEqual({
      sort: null, filters: { 'f:name': { values: [null, '', 'null'] } },
    });
  });
  test('table requests preserve existing search predicates and paging', () => {
    const url = new URL(tableUrl('/api/inventory/job/search/apps?name__contains=100%25&offset=100', {
      sort: { id: 'f:size', dir: 'desc' }, filters: {},
    }), 'http://x');
    expect(url.searchParams.get('name__contains')).toBe('100%');
    expect(url.searchParams.get('offset')).toBe('100');
    expect(url.searchParams.get('format')).toBe('page');
    expect(JSON.parse(url.searchParams.get('table')!).sort).toEqual({ id: 'f:size', dir: 'desc' });
  });
});
