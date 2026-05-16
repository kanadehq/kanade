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

type ResultRow = {
  request_id: string;
  pc_id: string;
  exit_code: number;
  stdout: string;
  stderr: string;
  started_at: string | null;
  finished_at: string | null;
};

const SINCE_PRESETS: Array<{ value: string; label: string; ms: number | null }> = [
  { value: '1h',  label: 'last 1h',   ms: 60 * 60 * 1000 },
  { value: '24h', label: 'last 24h',  ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  label: 'last 7d',   ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', label: 'last 30d',  ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', label: 'all time',  ms: null },
];

function fmt(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

export function Results() {
  const [pcId, setPcId] = useState('');
  const [status, setStatus] = useState<'' | 'success' | 'failure'>('');
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
    if (pcId)     sp.set('pc_id', pcId);
    if (status)   sp.set('status', status);
    if (sinceIso) sp.set('since', sinceIso);
    return sp.toString();
  }, [pcId, status, sinceIso, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['results', queryString],
    queryFn: () => apiFetch<ResultRow[]>(`/api/results?${queryString}`),
  });

  const rows = data ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Recent results</h2>
        <Badge variant="violet">{rows.length} shown{isFetching && !isLoading ? '…' : ''}</Badge>
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-4 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="res-pc">pc_id</Label>
            <Input
              id="res-pc"
              placeholder="exact match"
              value={pcId}
              onChange={(e) => setPcId(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-status">status</Label>
            <Select
              id="res-status"
              value={status}
              onChange={(e) => setStatus(e.target.value as '' | 'success' | 'failure')}
            >
              <option value="">(any)</option>
              <option value="success">success (exit 0)</option>
              <option value="failure">failure (exit ≠ 0)</option>
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-since">since</Label>
            <Select id="res-since" value={since} onChange={(e) => setSince(e.target.value)}>
              {SINCE_PRESETS.map((p) => (
                <option key={p.value} value={p.value}>{p.label}</option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-limit">limit</Label>
            <Select
              id="res-limit"
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
          <Loader2 className="size-4 animate-spin" />loading results…
        </div>
      ) : error ? (
        <ErrorCard title="Couldn't load results" error={error} />
      ) : rows.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>No results match</CardTitle></CardHeader>
          <CardContent className="text-muted">
            Widen the filter window or clear pc_id / status to see older runs.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>request_id</TableHead>
              <TableHead>pc_id</TableHead>
              <TableHead>exit</TableHead>
              <TableHead>started</TableHead>
              <TableHead>finished</TableHead>
              <TableHead>stdout / stderr</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((r) => (
              <TableRow key={r.request_id}>
                <TableCell><code className="text-xs">{r.request_id.slice(0, 8)}</code></TableCell>
                <TableCell><code className="text-xs">{r.pc_id}</code></TableCell>
                <TableCell>
                  <Badge variant={r.exit_code === 0 ? 'success' : 'danger'}>{r.exit_code}</Badge>
                </TableCell>
                <TableCell className="text-muted text-xs">{fmt(r.started_at)}</TableCell>
                <TableCell className="text-muted text-xs">{fmt(r.finished_at)}</TableCell>
                <TableCell className="max-w-md">
                  <pre className="text-xs whitespace-pre-wrap break-words bg-muted/5 p-2 rounded">
                    {(r.stdout || '(empty)').slice(0, 200)}
                  </pre>
                  {r.stderr && (
                    <pre className="text-xs whitespace-pre-wrap break-words text-danger bg-danger/5 p-2 rounded mt-1">
                      {r.stderr.slice(0, 200)}
                    </pre>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}
