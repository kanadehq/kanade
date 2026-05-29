import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

type EventRow = {
  id: number;
  pc_id: string;
  at: string;
  kind: string;
  source: string;
  event_record_id: string | null;
  payload: unknown;
};

type ListResponse = { events: EventRow[] };
type KindsResponse = { kinds: string[] };

const SINCE_PRESETS: Array<{ value: string; ms: number | null }> = [
  { value: '1h',  ms: 60 * 60 * 1000 },
  { value: '24h', ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', ms: null },
];

const FILTER_DEBOUNCE_MS = 300;

function useDebouncedValue<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), delay);
    return () => clearTimeout(t);
  }, [value, delay]);
  return debounced;
}

// Map the kind vocabulary spelt out in #246 onto the four existing
// badge palettes so an operator skimming the timeline can chunk
// lifecycle (success/danger) vs informational (violet) vs neutral
// (default) without reading every cell.
function kindVariant(kind: string): 'success' | 'amber' | 'danger' | 'violet' | 'default' {
  switch (kind) {
    case 'logon':
    case 'boot':
    case 'resume':
    case 'agent_started':
      return 'success';
    case 'logoff':
    case 'shutdown':
    case 'sleep':
      return 'default';
    case 'unexpected_shutdown':
      return 'danger';
    case 'diagnostic':
    case 'agent_self_update':
      return 'violet';
    default:
      return 'amber';
  }
}

export function Events() {
  const { t } = useTranslation('events');
  const [search, setSearch] = useSearchParams();
  const [pcId, setPcId] = useState(search.get('pc') ?? '');
  const [kind, setKind] = useState(search.get('kind') ?? '');
  const [source, setSource] = useState(search.get('source') ?? '');
  const [since, setSince] = useState(search.get('since') ?? '24h');
  const [limit, setLimit] = useState(Number(search.get('limit')) || 200);

  const sinceIso = useMemo(() => {
    const preset = SINCE_PRESETS.find((p) => p.value === since);
    if (!preset?.ms) return null;
    return new Date(Date.now() - preset.ms).toISOString();
  }, [since]);

  const dPcId   = useDebouncedValue(pcId,   FILTER_DEBOUNCE_MS);
  const dSource = useDebouncedValue(source, FILTER_DEBOUNCE_MS);

  // Mirror filters into the URL so a timeline drill-down link is
  // shareable / reload-safe (same shape as Logs). Uses the debounced
  // values for the typed-text inputs so a keystroke doesn't write a
  // partial URL on every change (Gemini #252 HIGH). `replace: true`
  // keeps these writes out of the back/forward stack, so polluting
  // history is a non-issue — no separate URL→state sync needed.
  useEffect(() => {
    const next = new URLSearchParams();
    if (dPcId)   next.set('pc', dPcId);
    if (kind)    next.set('kind', kind);
    if (dSource) next.set('source', dSource);
    if (since && since !== '24h') next.set('since', since);
    if (limit && limit !== 200)   next.set('limit', String(limit));
    setSearch(next, { replace: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dPcId, kind, dSource, since, limit]);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (dPcId)   sp.set('pc_id', dPcId);
    if (kind)    sp.set('kind', kind);
    if (dSource) sp.set('source', dSource);
    if (sinceIso) sp.set('from', sinceIso);
    return sp.toString();
  }, [dPcId, kind, dSource, sinceIso, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['obs_events', queryString],
    queryFn: () => apiFetch<ListResponse>(`/api/obs_events?${queryString}`),
  });

  // Populate the kind <select> from the backend's distinct list so the
  // operator can pick what's actually in the table instead of guessing
  // strings from the spec. Falls back to "(any)" when empty.
  const kindsQ = useQuery({
    queryKey: ['obs_events-kinds'],
    queryFn: () => apiFetch<KindsResponse>('/api/obs_events/kinds'),
    staleTime: 60_000,
  });

  const rows = data?.events ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <Badge variant="violet">
          {isFetching && !isLoading
            ? t('countBadgeFetching', { count: rows.length })
            : t('countBadge', { count: rows.length })}
        </Badge>
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="ev-pc">{t('filters.pcId')}</Label>
            <Input
              id="ev-pc"
              placeholder={t('filters.placeholders.pcId')}
              value={pcId}
              onChange={(e) => setPcId(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-kind">{t('filters.kind')}</Label>
            <Select id="ev-kind" value={kind} onChange={(e) => setKind(e.target.value)}>
              <option value="">{t('filters.kindOptions.any')}</option>
              {(kindsQ.data?.kinds ?? []).map((k) => (
                <option key={k} value={k}>{k}</option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-source">{t('filters.source')}</Label>
            <Input
              id="ev-source"
              placeholder={t('filters.placeholders.source')}
              value={source}
              onChange={(e) => setSource(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-since">{t('filters.since')}</Label>
            <Select id="ev-since" value={since} onChange={(e) => setSince(e.target.value)}>
              {SINCE_PRESETS.map((p) => (
                <option key={p.value} value={p.value}>
                  {t(`filters.sincePresets.${p.value}`)}
                </option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ev-limit">{t('filters.limit')}</Label>
            <Select
              id="ev-limit"
              value={String(limit)}
              onChange={(e) => setLimit(Number(e.target.value))}
            >
              <option value="50">50</option>
              <option value="200">200</option>
              <option value="1000">1000</option>
              <option value="5000">5000</option>
            </Select>
          </div>
        </CardContent>
      </Card>

      {isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />{t('loading')}
        </div>
      ) : error ? (
        <ErrorCard title={t('errorTitle')} error={error} />
      ) : rows.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>{t('empty.title')}</CardTitle></CardHeader>
          <CardContent className="text-muted">
            {t('empty.body')}
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>{t('columns.when')}</TableHead>
              <TableHead>{t('columns.pcId')}</TableHead>
              <TableHead>{t('columns.kind')}</TableHead>
              <TableHead>{t('columns.source')}</TableHead>
              <TableHead>{t('columns.recordId')}</TableHead>
              <TableHead>{t('columns.payload')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((e) => (
              <TableRow key={e.id}>
                <TableCell className="text-muted text-xs">{fmtIsoLocal(e.at)}</TableCell>
                <TableCell><code className="text-xs">{e.pc_id}</code></TableCell>
                <TableCell>
                  <Badge variant={kindVariant(e.kind)}>{e.kind}</Badge>
                </TableCell>
                <TableCell><code className="text-xs">{e.source}</code></TableCell>
                <TableCell>
                  {e.event_record_id
                    ? <code className="text-xs">{e.event_record_id}</code>
                    : <span className="text-muted text-xs">—</span>}
                </TableCell>
                <TableCell>
                  <details>
                    <summary className="cursor-pointer text-muted text-xs">{t('payload.show')}</summary>
                    <pre className="text-xs whitespace-pre-wrap break-words mt-2 bg-muted/5 p-2 rounded max-h-96 overflow-y-auto">
                      {JSON.stringify(e.payload, null, 2)}
                    </pre>
                  </details>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}
