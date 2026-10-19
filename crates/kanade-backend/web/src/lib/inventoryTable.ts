import { useCallback } from 'react';
import { useQueryClient } from '@tanstack/react-query';

import { apiFetch } from '@/lib/api';
import { parseFilters, parseSort, type ServerSortFilter, type TableQueryState } from '@/components/ui/table-sortfilter';

export const EMPTY_TABLE_STATE: TableQueryState = { sort: null, filters: {} };

// Checklist values are JSON-encoded internally: null and the literal string
// "null" are different values. Labels remain the original cell values.
export function encodeTableState(state: TableQueryState): string {
  const filters = Object.fromEntries(Object.entries(state.filters).map(([id, f]) => [id, {
    ...f, values: f.values?.map((v) => JSON.parse(v) as string | null),
  }]));
  return JSON.stringify({ ...state, filters });
}

export function parseTableState(raw: string | null): TableQueryState {
  try {
    const value = JSON.parse(raw ?? '{}');
    const filters = Object.fromEntries(Object.entries(value.filters ?? {}).map(([id, f]) => {
      const filter = f as { contains?: string; values?: (string | null)[] };
      return [id, { ...filter, values: filter.values?.map((v) => JSON.stringify(v)) }];
    }));
    return { sort: parseSort(value.sort), filters: parseFilters(filters) };
  } catch {
    return EMPTY_TABLE_STATE;
  }
}

export function tableUrl(url: string, state: TableQueryState): string {
  const parsed = new URL(url, 'http://x');
  parsed.searchParams.set('format', 'page');
  parsed.searchParams.set('table', encodeTableState(state));
  return `${parsed.pathname}?${parsed.searchParams}`;
}

export function useInventoryTable(
  url: string | null,
  state: TableQueryState,
  onChange: (state: TableQueryState) => void,
  total: number,
  loading: boolean,
): ServerSortFilter {
  const client = useQueryClient();
  const loadCandidates = useCallback(async (id: string) => {
    if (!url) return { values: [] };
    const parsed = new URL(url, 'http://x');
    parsed.searchParams.set('distinct', id);
    parsed.searchParams.delete('offset');
    parsed.searchParams.delete('limit');
    // Own-column conditions and row order do not affect candidates. Keep
    // their cache key stable while the operator edits this column's filter.
    const candidateState = parseTableState(parsed.searchParams.get('table'));
    const filters = { ...candidateState.filters };
    delete filters[id];
    parsed.searchParams.set('table', encodeTableState({ sort: null, filters }));
    const path = `${parsed.pathname}?${parsed.searchParams}`;
    const result = await client.fetchQuery({
      queryKey: ['inventory-distinct', path],
      staleTime: 30_000,
      queryFn: () => apiFetch<{
        values: { value: string | null; count: number }[];
        overflow: boolean;
      }>(path),
    });
    return {
      overflow: result.overflow,
      values: result.values.map((v) => ({
        value: JSON.stringify(v.value),
        label: v.value === '' ? '""' : v.value ?? '',
        count: v.count,
      })),
    };
  }, [client, url]);
  return {
    state, onChange, total, loading, loadCandidates,
    valueLabel: (value) => {
      try {
        const parsed = JSON.parse(value);
        return parsed === '' ? '""' : String(parsed ?? '');
      } catch {
        return value;
      }
    },
  };
}
