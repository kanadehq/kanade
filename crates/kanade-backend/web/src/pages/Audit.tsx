import { useQuery } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
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

function fmt(iso: string): string {
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

export function Audit() {
  const { data, error, isLoading } = useQuery({
    queryKey: ['audit'],
    queryFn: () => apiFetch<AuditRow[]>('/api/audit?limit=50'),
  });

  if (isLoading) return <div className="flex items-center gap-2 text-muted"><Loader2 className="size-4 animate-spin" />loading audit log…</div>;
  if (error) return <ErrorCard title="Couldn't load audit log" error={error} />;
  const rows = data ?? [];

  if (rows.length === 0) {
    return (
      <Card>
        <CardHeader><CardTitle>No audit events yet</CardTitle></CardHeader>
        <CardContent className="text-muted">
          Operator actions (deploys, schedules, revokes) get audited here once executed.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Audit log</h2>
        <Badge variant="violet">{rows.length} shown</Badge>
      </div>
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
    </div>
  );
}
