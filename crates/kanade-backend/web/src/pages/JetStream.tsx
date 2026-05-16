import { useQuery } from '@tanstack/react-query';
import { CheckCircle2, Loader2, XCircle } from 'lucide-react';

import { ErrorCard } from '@/components/ErrorCard';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { JetstreamSnapshot } from '@/lib/types';

function ProbeTable({ rows }: { rows: { name: string; exists: boolean }[] }) {
  return (
    <Table>
      <TableHeader>
        <TableRow><TableHead>name</TableHead><TableHead>status</TableHead></TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((r) => (
          <TableRow key={r.name}>
            <TableCell><code className="text-xs">{r.name}</code></TableCell>
            <TableCell>
              {r.exists
                ? <span className="inline-flex items-center gap-1 text-success"><CheckCircle2 className="size-4" />ok</span>
                : <span className="inline-flex items-center gap-1 text-danger"><XCircle className="size-4" />missing</span>}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

export function JetStream() {
  const { data, error, isLoading } = useQuery({
    queryKey: ['jetstream-status'],
    queryFn: () => apiFetch<JetstreamSnapshot>('/api/jetstream/status'),
  });

  if (isLoading) return <div className="flex items-center gap-2 text-muted"><Loader2 className="size-4 animate-spin" />loading…</div>;
  if (error) return <ErrorCard title="Couldn't load JetStream status" error={error} />;
  if (!data) return null;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>JetStream health</CardTitle>
          <CardDescription>
            Live probe of <code>/api/jetstream/status</code>. Missing rows immediately after broker boot are
            normal — backend auto-bootstraps every resource on startup.
          </CardDescription>
        </CardHeader>
      </Card>
      <section className="space-y-2">
        <h3 className="text-base font-bold">Streams</h3>
        <ProbeTable rows={data.streams} />
      </section>
      <section className="space-y-2">
        <h3 className="text-base font-bold">KV buckets</h3>
        <ProbeTable rows={data.kv_buckets} />
      </section>
      <section className="space-y-2">
        <h3 className="text-base font-bold">Object stores</h3>
        <ProbeTable rows={data.object_stores} />
      </section>
    </div>
  );
}
