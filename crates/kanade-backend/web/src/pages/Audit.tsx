import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { useDebouncedValue } from '@/lib/hooks';
import { fmtIsoLocal } from '@/lib/utils';

type AuditRow = {
  id: number;
  actor: string;
  action: string;
  target: string | null;
  payload: unknown;
  occurred_at: string;
};

// #523: same debounce the Activity / Events filter inputs use.
const FILTER_DEBOUNCE_MS = 300;

const SINCE_PRESETS: Array<{ value: string; ms: number | null }> = [
  { value: '1h',  ms: 60 * 60 * 1000 },
  { value: '24h', ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', ms: null },
];

function actorVariant(actor: string): 'violet' | 'amber' | 'success' | 'default' {
  switch (actor) {
    case 'scheduler':   return 'violet';
    case 'operator':    return 'amber';
    case 'self-update': return 'success';
    case 'agent':       return 'default';
    default:            return 'default'; // future / legacy values
  }
}

export function Audit() {
  const { t } = useTranslation('audit');
  const [actor, setActor] = useState('');
  const [action, setAction] = useState('');
  const [target, setTarget] = useState('');
  const [payload, setPayload] = useState('');
  const [since, setSince] = useState('24h');
  const [limit, setLimit] = useState(50);

  // #519: only the preset's window LENGTH lives in render — the
  // `since` lower bound is computed inside queryFn (the HistoryPane
  // pattern) so each refetch re-anchors to Date.now() instead of the
  // moment the preset was picked.
  const sinceMs = useMemo(
    () => SINCE_PRESETS.find((p) => p.value === since)?.ms ?? null,
    [since],
  );

  // #523: debounce the typed filters before they hit the queryKey —
  // the payload filter especially routes into the backend's
  // expensive regex prefilter path, and previously every keystroke
  // fired a fresh query. Same 300 ms the Activity / Events inputs use.
  const dActor   = useDebouncedValue(actor, FILTER_DEBOUNCE_MS);
  const dAction  = useDebouncedValue(action, FILTER_DEBOUNCE_MS);
  const dTarget  = useDebouncedValue(target, FILTER_DEBOUNCE_MS);
  const dPayload = useDebouncedValue(payload, FILTER_DEBOUNCE_MS);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (dActor)   sp.set('actor', dActor);
    if (dAction)  sp.set('action', dAction);
    if (dTarget)  sp.set('target', dTarget);
    if (dPayload) sp.set('payload', dPayload);
    return sp.toString();
  }, [dActor, dAction, dTarget, dPayload, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    // Preset key (not a computed ISO) keeps the cache partitioned
    // per window without millisecond-tick invalidation (#519).
    queryKey: ['audit', queryString, since],
    queryFn: () => {
      const sp = new URLSearchParams(queryString);
      if (sinceMs) sp.set('since', new Date(Date.now() - sinceMs).toISOString());
      return apiFetch<AuditRow[]>(`/api/audit?${sp.toString()}`);
    },
  });

  const rows = data ?? [];

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
            <Label htmlFor="audit-actor">{t('filters.actor')}</Label>
            <Select id="audit-actor" value={actor} onChange={(e) => setActor(e.target.value)}>
              <option value="">{t('filters.actorOptions.any')}</option>
              <option value="scheduler">{t('filters.actorOptions.scheduler')}</option>
              <option value="operator">{t('filters.actorOptions.operator')}</option>
              <option value="self-update">{t('filters.actorOptions.selfUpdate')}</option>
              <option value="agent">{t('filters.actorOptions.agent')}</option>
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-action">{t('filters.action')}</Label>
            <Input
              id="audit-action"
              placeholder={t('filters.placeholders.action')}
              value={action}
              onChange={(e) => setAction(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-target">{t('filters.target')}</Label>
            <Input
              id="audit-target"
              placeholder={t('filters.placeholders.target')}
              value={target}
              onChange={(e) => setTarget(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-payload">{t('filters.payload')}</Label>
            <Input
              id="audit-payload"
              placeholder={t('filters.placeholders.payload')}
              value={payload}
              onChange={(e) => setPayload(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-since">{t('filters.since')}</Label>
            <Select id="audit-since" value={since} onChange={(e) => setSince(e.target.value)}>
              {SINCE_PRESETS.map((p) => (
                <option key={p.value} value={p.value}>
                  {t(`filters.sincePresets.${p.value}`)}
                </option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-limit">{t('filters.limit')}</Label>
            <Select
              id="audit-limit"
              value={String(limit)}
              onChange={(e) => setLimit(Number(e.target.value))}
            >
              <option value="50">50</option>
              <option value="200">200</option>
              <option value="1000">1000</option>
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
              <TableHead>{t('columns.actor')}</TableHead>
              <TableHead>{t('columns.action')}</TableHead>
              <TableHead>{t('columns.target')}</TableHead>
              <TableHead>{t('columns.payload')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((e) => (
              <TableRow key={e.id}>
                <TableCell label={t('columns.when')} className="text-muted text-xs">{fmtIsoLocal(e.occurred_at)}</TableCell>
                <TableCell label={t('columns.actor')}>
                  <Badge variant={actorVariant(e.actor)}>{e.actor}</Badge>
                </TableCell>
                <TableCell label={t('columns.action')}><code className="text-xs">{e.action}</code></TableCell>
                <TableCell label={t('columns.target')}><code className="text-xs">{e.target ?? '—'}</code></TableCell>
                <TableCell label={t('columns.payload')}>
                  <details>
                    <summary className="cursor-pointer text-muted text-xs">{t('payload.show')}</summary>
                    <pre className="text-xs whitespace-pre-wrap break-words mt-2 bg-muted/5 p-2 rounded">
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
