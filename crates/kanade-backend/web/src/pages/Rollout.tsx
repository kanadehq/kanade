import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { CheckCircle2, Loader2, Rocket } from 'lucide-react';
import { useState } from 'react';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import type { AgentRow } from '@/lib/types';

type ReleaseRow = {
  version: string;
  size: number;
  digest: string | null;
  modified: string | null;
};

type ScopeKind = 'global' | 'group' | 'pc';

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function fmtTime(iso: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

export function Rollout() {
  const qc = useQueryClient();
  const [version, setVersion] = useState('');
  const [scopeKind, setScopeKind] = useState<ScopeKind>('group');
  const [scopeValue, setScopeValue] = useState('canary');
  const [jitter, setJitter] = useState('5m');

  const releasesQ = useQuery({
    queryKey: ['agent-releases'],
    queryFn: () => apiFetch<ReleaseRow[]>('/api/agents/releases'),
  });
  const agentsQ = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
  });

  const rollout = useMutation({
    mutationFn: async () => {
      type Scope = { type: 'global' } | { type: 'group'; value: string } | { type: 'pc'; value: string };
      const scope: Scope =
        scopeKind === 'global'
          ? { type: 'global' }
          : scopeKind === 'group'
          ? { type: 'group', value: scopeValue }
          : { type: 'pc', value: scopeValue };
      return apiFetch<{ version: string; scope_label: string; scope_key: string; jitter: string | null }>(
        '/api/agents/rollout',
        {
          method: 'POST',
          body: JSON.stringify({
            version,
            scope,
            jitter: jitter || undefined,
          }),
        },
      );
    },
    onSuccess: () => {
      // Resolve effective config will move; nudge dependent queries.
      qc.invalidateQueries({ queryKey: ['effective'] });
    },
  });

  const canSubmit =
    !!version &&
    (scopeKind === 'global' || (!!scopeValue && scopeValue.length > 0)) &&
    !rollout.isPending;

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Agent rollout</h2>
        <span className="text-xs text-muted">
          Two-step: <code>kanade agent publish</code> uploads the binary, this panel flips <code>target_version</code> on one scope.
        </span>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Rocket className="size-5 text-violet" />
            Roll out a version
          </CardTitle>
          <CardDescription>
            Pick a version from the Object Store, choose a scope, and (recommended) set a jitter so agents don't all download at the same instant.
          </CardDescription>
        </CardHeader>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          <div className="space-y-1">
            <Label htmlFor="ro-version">version</Label>
            <Select id="ro-version" value={version} onChange={(e) => setVersion(e.target.value)}>
              <option value="">(pick one)</option>
              {(releasesQ.data ?? []).map((r) => (
                <option key={r.version} value={r.version}>
                  {r.version} · {fmtSize(r.size)} · {fmtTime(r.modified)}
                </option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ro-jitter">jitter (humantime)</Label>
            <Input
              id="ro-jitter"
              placeholder="e.g. 5m, 30m, 1h, 0s"
              value={jitter}
              onChange={(e) => setJitter(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="ro-scope-kind">scope</Label>
            <Select
              id="ro-scope-kind"
              value={scopeKind}
              onChange={(e) => setScopeKind(e.target.value as ScopeKind)}
            >
              <option value="global">global (whole fleet)</option>
              <option value="group">group</option>
              <option value="pc">single PC</option>
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="ro-scope-value">
              {scopeKind === 'global' ? '—' : scopeKind === 'group' ? 'group name' : 'pc_id'}
            </Label>
            {scopeKind === 'pc' ? (
              <Select
                id="ro-scope-value"
                value={scopeValue}
                onChange={(e) => setScopeValue(e.target.value)}
              >
                <option value="">(pick one)</option>
                {(agentsQ.data ?? []).map((a) => (
                  <option key={a.pc_id} value={a.pc_id}>{a.pc_id}</option>
                ))}
              </Select>
            ) : (
              <Input
                id="ro-scope-value"
                placeholder={scopeKind === 'global' ? 'n/a' : 'e.g. canary'}
                value={scopeValue}
                onChange={(e) => setScopeValue(e.target.value)}
                disabled={scopeKind === 'global'}
              />
            )}
          </div>
        </CardContent>
        <CardContent className="flex items-center gap-3 pt-0">
          <Button
            onClick={() => rollout.mutate()}
            disabled={!canSubmit}
          >
            {rollout.isPending ? (
              <Loader2 className="size-4 mr-2 animate-spin" />
            ) : (
              <Rocket className="size-4 mr-2" />
            )}
            Roll out
          </Button>
          {rollout.isSuccess && rollout.data && (
            <span className="flex items-center gap-2 text-sm text-success">
              <CheckCircle2 className="size-4" />
              {rollout.data.scope_label} → {rollout.data.version}
              {rollout.data.jitter && (
                <Badge variant="violet">jitter {rollout.data.jitter}</Badge>
              )}
            </span>
          )}
        </CardContent>
        {rollout.error && (
          <CardContent className="pt-0">
            <ErrorCard title="Rollout failed" error={rollout.error} />
          </CardContent>
        )}
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Object Store releases</CardTitle>
          <CardDescription>From <code>/api/agents/releases</code> — what's available to rollout.</CardDescription>
        </CardHeader>
        <CardContent>
          {releasesQ.isLoading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="size-4 animate-spin" />loading…
            </div>
          ) : releasesQ.error ? (
            <ErrorCard title="Couldn't load releases" error={releasesQ.error} />
          ) : (releasesQ.data ?? []).length === 0 ? (
            <div className="text-muted text-sm">
              Object Store empty — run <code>kanade agent publish &lt;binary&gt;</code> first.
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>version</TableHead>
                  <TableHead>size</TableHead>
                  <TableHead>modified</TableHead>
                  <TableHead>digest</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(releasesQ.data ?? []).map((r) => (
                  <TableRow key={r.version}>
                    <TableCell><code className="text-xs">{r.version}</code></TableCell>
                    <TableCell className="text-muted text-xs">{fmtSize(r.size)}</TableCell>
                    <TableCell className="text-muted text-xs">{fmtTime(r.modified)}</TableCell>
                    <TableCell className="text-muted text-xs">
                      <code>{r.digest?.slice(0, 24) ?? '—'}</code>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
