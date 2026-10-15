import { useEffect, useState } from 'react';
import { useSearchParams } from 'react-router-dom';

const FILTER_KEYS = ['job_id', 'pc_id', 'exec_id'] as const;
type FilterKey = typeof FILTER_KEYS[number];
type Filters = Record<FilterKey, string>;

export function useActivityFilters() {
  const [params, setParams] = useSearchParams();
  const url = params.toString();
  const fromUrl: Filters = {
    job_id: params.get('job_id') ?? '',
    pc_id: params.get('pc_id') ?? '',
    exec_id: params.get('exec_id') ?? '',
  };
  const [draft, setDraft] = useState({ url, values: fromUrl });
  const values = draft.url === url ? draft.values : fromUrl;

  // Discard drafts from the previous location synchronously. In particular,
  // back/forward must never let an old debounce overwrite the new URL.
  if (draft.url !== url) setDraft({ url, values: fromUrl });

  useEffect(() => {
    if (draft.url !== url
      || FILTER_KEYS.every((key) => draft.values[key] === (params.get(key) ?? ''))) return;
    const timer = setTimeout(() => {
      setParams((prev) => {
        if (prev.toString() !== url) return prev;
        const next = new URLSearchParams(prev);
        for (const key of FILTER_KEYS) {
          if (draft.values[key]) next.set(key, draft.values[key]);
          else next.delete(key);
        }
        return next;
      }, { replace: true });
    }, 300);
    return () => clearTimeout(timer);
  }, [draft, url, params, setParams]);

  const setFilter = (key: FilterKey, value: string) => {
    setDraft({ url, values: { ...values, [key]: value } });
  };
  // Queries follow the committed URL; local edits reach it after the debounce.
  return { values, filters: fromUrl, setFilter };
}
