import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
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

function fmt(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

export function Results() {
  const { data, error, isLoading } = useQuery({
    queryKey: ['results'],
    queryFn: () => apiFetch<ResultRow[]>('/api/results?limit=50'),
  });

  if (isLoading) return <div className="flex items-center gap-2 text-muted"><Loader2 className="size-4 animate-spin" />loading results…</div>;
  if (error) return <ErrorCard title="Couldn't load results" error={error} />;
  const rows = data ?? [];

  if (rows.length === 0) {
    return (
      <Card>
        <CardHeader><CardTitle>No results yet</CardTitle></CardHeader>
        <CardContent className="text-muted">
          Run a deploy or use the Run page to send a script — its ExecResult will land here.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Recent results</h2>
        <Badge variant="violet">{rows.length} shown</Badge>
      </div>
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
    </div>
  );
}
