import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { CheckCircle2, Loader2, Rocket, Trash2, Upload } from 'lucide-react';
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
  const [uploadFile, setUploadFile] = useState<File | null>(null);

  const releasesQ = useQuery({
    queryKey: ['agent-releases'],
    queryFn: () => apiFetch<ReleaseRow[]>('/api/agents/releases'),
  });
  const agentsQ = useQuery({
    queryKey: ['agents'],
    queryFn: () => apiFetch<AgentRow[]>('/api/agents'),
  });

  const upload = useMutation({
    mutationFn: async () => {
      if (!uploadFile) throw new Error('pick a file first');
      const fd = new FormData();
      fd.append('file', uploadFile);
      const token = localStorage.getItem('kanade_token') ?? '';
      const headers: Record<string, string> = { 'X-Kanade-Source': 'spa' };
      if (token) headers.Authorization = `Bearer ${token}`;
      const res = await fetch('/api/agents/publish', {
        method: 'POST',
        body: fd,
        headers,
      });
      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText} — ${await res.text()}`);
      }
      return (await res.json()) as { version: string; size: number; digest: string | null };
    },
    onSuccess: (data) => {
      qc.invalidateQueries({ queryKey: ['agent-releases'] });
      // Pre-select the freshly uploaded version for the rollout
      // step right below.
      setVersion(data.version);
      setUploadFile(null);
    },
  });

  const rollout = useMutation({
    mutationFn: async (overrideVersion?: string) => {
      const ver = overrideVersion ?? version;
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
            version: ver,
            scope,
            jitter: jitter || undefined,
          }),
        },
      );
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['effective'] });
    },
  });

  const remove = useMutation({
    mutationFn: async (v: string) => {
      const token = localStorage.getItem('kanade_token') ?? '';
      const headers: Record<string, string> = { 'X-Kanade-Source': 'spa' };
      if (token) headers.Authorization = `Bearer ${token}`;
      const res = await fetch(`/api/agents/releases/${encodeURIComponent(v)}`, {
        method: 'DELETE',
        headers,
      });
      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText} — ${await res.text()}`);
      }
      return v;
    },
    onSuccess: (v) => {
      qc.invalidateQueries({ queryKey: ['agent-releases'] });
      if (version === v) setVersion('');
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
          Two-step: upload a binary, then flip <code>target_version</code> on one scope.
        </span>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Upload className="size-5 text-violet" />
            1. Upload a binary
          </CardTitle>
          <CardDescription>
            POSTs to <code>/api/agents/publish</code> (64 MB body limit). The Object
            Store key is auto-extracted from the binary's embedded VERSIONINFO
            resource — no label to type, no chance of a label/binary mismatch.
            CLI: <code>kanade agent publish &lt;binary&gt;</code>.
          </CardDescription>
        </CardHeader>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          <div className="space-y-1">
            <Label htmlFor="up-file">binary (.exe / Linux / macOS)</Label>
            <Input
              id="up-file"
              type="file"
              accept=".exe,application/octet-stream,application/x-msdownload"
              onChange={(e) => setUploadFile(e.target.files?.[0] ?? null)}
            />
          </div>
          <div className="space-y-1 flex flex-col justify-end">
            <Button
              onClick={() => upload.mutate()}
              disabled={!uploadFile || upload.isPending}
              className="w-full"
            >
              {upload.isPending ? (
                <Loader2 className="size-4 mr-2 animate-spin" />
              ) : (
                <Upload className="size-4 mr-2" />
              )}
              Upload
            </Button>
          </div>
        </CardContent>
        <CardContent className="flex items-center gap-3 pt-0">
          {uploadFile && (
            <span className="text-xs text-muted">
              {uploadFile.name} · {fmtSize(uploadFile.size)}
            </span>
          )}
          {upload.isSuccess && upload.data && (
            <span className="flex items-center gap-2 text-sm text-success">
              <CheckCircle2 className="size-4" />
              uploaded as <code className="text-xs">{upload.data.version}</code>
              {' '}({fmtSize(upload.data.size)})
            </span>
          )}
        </CardContent>
        {upload.error && (
          <CardContent className="pt-0">
            <ErrorCard title="Upload failed" error={upload.error} />
          </CardContent>
        )}
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Rocket className="size-5 text-violet" />
            2. Roll out a version
          </CardTitle>
          <CardDescription>
            Or click <strong>Roll out</strong> on a row in the table below to pre-fill the form.
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
          <Button onClick={() => rollout.mutate(undefined)} disabled={!canSubmit}>
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
          <CardDescription>
            From <code>/api/agents/releases</code>. Click <strong>Roll out</strong> to
            point the form above at a row; <strong>Delete</strong> removes the binary
            from the Object Store (refused with 409 if any scope still targets it).
          </CardDescription>
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
              Object Store empty — upload a binary above (or run{' '}
              <code>kanade agent publish &lt;binary&gt;</code>).
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>version</TableHead>
                  <TableHead>size</TableHead>
                  <TableHead>modified</TableHead>
                  <TableHead>digest</TableHead>
                  <TableHead className="text-right">actions</TableHead>
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
                    <TableCell className="text-right">
                      <div className="flex justify-end gap-2">
                        <Button
                          size="sm"
                          variant="secondary"
                          onClick={() => setVersion(r.version)}
                        >
                          <Rocket className="size-3.5" />
                          Roll out…
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          onClick={() => {
                            if (confirm(`Delete release ${r.version}? This cannot be undone.`)) {
                              remove.mutate(r.version);
                            }
                          }}
                          disabled={remove.isPending}
                        >
                          <Trash2 className="size-3.5" />
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
          {remove.error && (
            <div className="mt-3">
              <ErrorCard title="Delete failed" error={remove.error} />
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
