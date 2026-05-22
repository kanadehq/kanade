import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Save, Trash2 } from 'lucide-react';
import { useEffect, useState } from 'react';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Textarea } from '@/components/ui/textarea';
import { apiFetch, formatError } from '@/lib/api';
import type { ConfigScope, EffectiveConfigResponse } from '@/lib/types';

function GlobalEditor() {
  const qc = useQueryClient();
  const { data, error, isLoading } = useQuery({
    queryKey: ['config', 'global'],
    queryFn: () => apiFetch<ConfigScope>('/api/config'),
  });
  const [draft, setDraft] = useState<string>('');
  useEffect(() => {
    if (data) setDraft(JSON.stringify(data, null, 2));
  }, [data]);

  const save = useMutation({
    mutationFn: (body: ConfigScope) =>
      apiFetch<ConfigScope>('/api/config', { method: 'PUT', body: JSON.stringify(body) }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['config', 'global'] });
      toast.success('Saved global config');
    },
    onError: (e) => toast.error(`Save failed: ${formatError(e)}`),
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Global ConfigScope</CardTitle>
        <CardDescription>
          Whole-fleet default. Per-group / per-pc overrides win over this.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {isLoading && <div className="text-muted flex items-center gap-2"><Loader2 className="size-4 animate-spin" />loading…</div>}
        {error && <ErrorCard title="Couldn't load global config" error={error} />}
        <Textarea value={draft} onChange={(e) => setDraft(e.target.value)} className="min-h-40" />
        <Button
          onClick={() => {
            try {
              save.mutate(JSON.parse(draft));
            } catch (e) {
              window.alert(`Body must be valid JSON: ${(e as Error).message}`);
            }
          }}
          disabled={save.isPending}
        >
          {save.isPending ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
          save global
        </Button>
        {save.error && <ErrorCard title="Save failed" error={save.error} />}
      </CardContent>
    </Card>
  );
}

function ScopeEditor() {
  const qc = useQueryClient();
  const confirm = useConfirm();
  const [kind, setKind] = useState<'groups' | 'pcs'>('groups');
  const [name, setName] = useState('');
  const [body, setBody] = useState('');
  const url = () => (kind === 'groups' ? `/api/groups/${encodeURIComponent(name)}/config` : `/api/pcs/${encodeURIComponent(name)}/config`);

  const load = useMutation({
    mutationFn: () => apiFetch<ConfigScope>(url()),
    onSuccess: (r) => setBody(JSON.stringify(r, null, 2)),
  });
  const save = useMutation({
    mutationFn: (b: ConfigScope) => apiFetch(url(), { method: 'PUT', body: JSON.stringify(b) }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['config'] });
      toast.success(`Saved ${url()}`);
    },
    onError: (e) => toast.error(`Save failed: ${formatError(e)}`),
  });
  const del = useMutation({
    mutationFn: () => apiFetch(url(), { method: 'DELETE' }),
    onSuccess: () => {
      setBody('');
      qc.invalidateQueries({ queryKey: ['config'] });
      toast.success(`Deleted ${url()}`);
    },
    onError: (e) => toast.error(`Delete failed: ${formatError(e)}`),
  });

  const lastError = load.error ?? save.error ?? del.error;

  return (
    <Card>
      <CardHeader>
        <CardTitle>Per-group / Per-PC override</CardTitle>
        <CardDescription>
          Partial ConfigScope — only fields you set survive. Empty fields fall through to the global scope.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="grid grid-cols-1 md:grid-cols-[160px_1fr_auto] gap-3 items-end">
          <div>
            <Label>scope</Label>
            <Select value={kind} onChange={(e) => setKind(e.target.value as 'groups' | 'pcs')}>
              <option value="groups">groups.&lt;name&gt;</option>
              <option value="pcs">pcs.&lt;pc_id&gt;</option>
            </Select>
          </div>
          <div>
            <Label>name</Label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="canary or MINIPC-01" />
          </div>
          <Button variant="secondary" disabled={!name.trim() || load.isPending} onClick={() => load.mutate()}>
            load
          </Button>
        </div>
        <Textarea
          value={body}
          onChange={(e) => setBody(e.target.value)}
          className="min-h-32"
          placeholder='{"target_version":"0.4.0"}'
        />
        <div className="flex gap-2">
          <Button
            onClick={() => {
              try {
                save.mutate(JSON.parse(body));
              } catch (e) {
                window.alert(`Body must be valid JSON: ${(e as Error).message}`);
              }
            }}
            disabled={!name.trim() || save.isPending}
          >
            {save.isPending ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
            save
          </Button>
          <Button
            variant="danger"
            disabled={!name.trim() || del.isPending}
            onClick={async () => {
              const ok = await confirm({
                title: `Delete ${url()}?`,
                description: 'Removes the override; the scope falls back through the layered config.',
                confirmLabel: 'Delete',
                danger: true,
              });
              if (ok) del.mutate();
            }}
          >
            <Trash2 className="size-3.5" />
            delete
          </Button>
        </div>
        {lastError && <ErrorCard title="Scope op failed" error={lastError} />}
      </CardContent>
    </Card>
  );
}

function EffectiveResolver() {
  const [pcId, setPcId] = useState('');
  const mut = useMutation({
    mutationFn: () => apiFetch<EffectiveConfigResponse>(`/api/agents/${encodeURIComponent(pcId)}/effective_config`),
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Effective config for one PC</CardTitle>
        <CardDescription>
          Same view the agent's config_supervisor computes locally. Built-in → global → groups (last-wins) → pc.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex gap-3 items-end">
          <div className="flex-1">
            <Label>pc_id</Label>
            <Input value={pcId} onChange={(e) => setPcId(e.target.value)} placeholder="MINIPC-01" />
          </div>
          <Button onClick={() => mut.mutate()} disabled={!pcId.trim() || mut.isPending} variant="secondary">
            resolve
          </Button>
        </div>
        {mut.error && <ErrorCard title="Resolve failed" error={mut.error} />}
        {mut.data && (
          <div className="space-y-2">
            <JsonOutput value={mut.data.effective} />
            {mut.data.warnings.length > 0 && (
              <div className="text-xs">
                <Label>warnings</Label>
                <ul className="list-disc pl-5 text-amber">
                  {mut.data.warnings.map((w, i) => <li key={i}>{w}</li>)}
                </ul>
              </div>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function Config() {
  return (
    <div className="space-y-4">
      <GlobalEditor />
      <ScopeEditor />
      <EffectiveResolver />
    </div>
  );
}
