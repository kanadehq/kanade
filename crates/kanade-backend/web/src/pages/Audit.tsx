import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useMemo, useState } from 'react';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';

type AuditRow = {
  id: number;
  actor: string;
  action: string;
  target: string | null;
  payload: unknown;
  occurred_at: string;
};

const SINCE_PRESETS: Array<{ value: string; label: string; ms: number | null }> = [
  { value: '1h',  label: 'last 1h',   ms: 60 * 60 * 1000 },
  { value: '24h', label: 'last 24h',  ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  label: 'last 7d',   ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', label: 'last 30d',  ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', label: 'all time',  ms: null },
];

function fmt(iso: string): string {
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

export function Audit() {
  const [actor, setActor] = useState('');
  const [action, setAction] = useState('');
  const [since, setSince] = useState('24h');
  const [limit, setLimit] = useState(50);

  const sinceIso = useMemo(() => {
    const preset = SINCE_PRESETS.find((p) => p.value === since);
    if (!preset?.ms) return null;
    return new Date(Date.now() - preset.ms).toISOString();
  }, [since]);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (actor)    sp.set('actor', actor);
    if (action)   sp.set('action', action);
    if (sinceIso) sp.set('since', sinceIso);
    return sp.toString();
  }, [actor, action, sinceIso, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['audit', queryString],
    queryFn: () => apiFetch<AuditRow[]>(`/api/audit?${queryString}`),
  });

  const rows = data ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Audit log</h2>
        <Badge variant="violet">{rows.length} shown{isFetching && !isLoading ? '…' : ''}</Badge>
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-4 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="audit-actor">actor</Label>
            <Select id="audit-actor" value={actor} onChange={(e) => setActor(e.target.value)}>
              <option value="">(any)</option>
              <option value="scheduler">scheduler</option>
              <option value="operator">operator</option>
              <option value="self-update">self-update</option>
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-action">action</Label>
            <Input
              id="audit-action"
              placeholder="exact match — eg. exec"
              value={action}
              onChange={(e) => setAction(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-since">since</Label>
            <Select id="audit-since" value={since} onChange={(e) => setSince(e.target.value)}>
              {SINCE_PRESETS.map((p) => (
                <option key={p.value} value={p.value}>{p.label}</option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="audit-limit">limit</Label>
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
          <Loader2 className="size-4 animate-spin" />loading audit log…
        </div>
      ) : error ? (
        <ErrorCard title="Couldn't load audit log" error={error} />
      ) : rows.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>No audit events match</CardTitle></CardHeader>
          <CardContent className="text-muted">
            Widen the filter window or clear actor / action to see older events.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>when</TableHead>
              <TableHead>actor</TableHead>
              <TableHead>action</TableHead>
              <TableHead>target</TableHead>
              <TableHead>payload</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((e) => (
              <TableRow key={e.id}>
                <TableCell className="text-muted text-xs">{fmt(e.occurred_at)}</TableCell>
                <TableCell>
                  <Badge variant={e.actor === 'scheduler' ? 'violet' : 'amber'}>{e.actor}</Badge>
                </TableCell>
                <TableCell><code className="text-xs">{e.action}</code></TableCell>
                <TableCell><code className="text-xs">{e.target ?? '—'}</code></TableCell>
                <TableCell>
                  <details>
                    <summary className="cursor-pointer text-muted text-xs">show</summary>
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
