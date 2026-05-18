import { useMutation, useQuery } from '@tanstack/react-query';
import { Loader2, Send } from 'lucide-react';
import { useState } from 'react';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { apiFetch } from '@/lib/api';

type JobRow = { id: string; version: string; description: string | null };

type ExecResponse = {
  exec_id: string;
  job_id: string;
  version: string;
  target_count: number;
  subjects: string[];
};

type TargetMode = 'all' | 'groups' | 'pcs';

type FanoutPlan = {
  target: { all: boolean; groups: string[]; pcs: string[] };
  jitter?: string;
};

function splitCsv(s: string): string[] {
  return s
    .split(',')
    .map((x) => x.trim())
    .filter(Boolean);
}

export function Exec() {
  const [jobId, setJobId] = useState('');
  const [mode, setMode] = useState<TargetMode>('all');
  const [groups, setGroups] = useState('');
  const [pcs, setPcs] = useState('');
  const [jitter, setJitter] = useState('');

  const jobsQ = useQuery({
    queryKey: ['jobs'],
    queryFn: () => apiFetch<JobRow[]>('/api/jobs'),
  });

  const mut = useMutation({
    mutationFn: ({ id, plan }: { id: string; plan: FanoutPlan }) =>
      apiFetch<ExecResponse>(`/api/exec/${encodeURIComponent(id)}`, {
        method: 'POST',
        body: JSON.stringify(plan),
      }),
  });

  const jobs = jobsQ.data ?? [];

  const onFire = () => {
    const plan: FanoutPlan = {
      target: {
        all: mode === 'all',
        groups: mode === 'groups' ? splitCsv(groups) : [],
        pcs: mode === 'pcs' ? splitCsv(pcs) : [],
      },
    };
    if (jitter.trim()) plan.jitter = jitter.trim();
    mut.mutate({ id: jobId, plan });
  };

  const targetReady =
    mode === 'all'
      || (mode === 'groups' && splitCsv(groups).length > 0)
      || (mode === 'pcs' && splitCsv(pcs).length > 0);

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>Exec a registered job</CardTitle>
          <CardDescription>
            Posts to <code>/api/exec/&lt;job_id&gt;</code> with the chosen
            target. Register new jobs with{' '}
            <code>kanade job create &lt;manifest.yaml&gt;</code>; wave rollouts
            live on the schedule yaml side, not on ad-hoc exec.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {jobsQ.isLoading ? (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="size-4 animate-spin" />loading jobs…
            </div>
          ) : jobsQ.error ? (
            <ErrorCard title="Couldn't load jobs" error={jobsQ.error} />
          ) : jobs.length === 0 ? (
            <div className="text-muted text-sm">
              No registered jobs. Run <code>kanade job create &lt;manifest.yaml&gt;</code>{' '}
              first.
            </div>
          ) : (
            <>
              <div className="space-y-1">
                <Label htmlFor="exec-job">job_id</Label>
                <Select
                  id="exec-job"
                  value={jobId}
                  onChange={(e) => setJobId(e.target.value)}
                >
                  <option value="">(pick one)</option>
                  {jobs.map((j) => (
                    <option key={j.id} value={j.id}>
                      {j.id} — v{j.version}
                      {j.description ? ` · ${j.description}` : ''}
                    </option>
                  ))}
                </Select>
              </div>

              <div className="space-y-1">
                <Label htmlFor="exec-target">target</Label>
                <Select
                  id="exec-target"
                  value={mode}
                  onChange={(e) => setMode(e.target.value as TargetMode)}
                >
                  <option value="all">all agents</option>
                  <option value="groups">specific group(s)</option>
                  <option value="pcs">specific pc(s)</option>
                </Select>
              </div>

              {mode === 'groups' && (
                <div className="space-y-1">
                  <Label htmlFor="exec-groups">groups (comma-separated)</Label>
                  <Input
                    id="exec-groups"
                    value={groups}
                    onChange={(e) => setGroups(e.target.value)}
                    placeholder="canary,wave1"
                  />
                </div>
              )}
              {mode === 'pcs' && (
                <div className="space-y-1">
                  <Label htmlFor="exec-pcs">pc_ids (comma-separated)</Label>
                  <Input
                    id="exec-pcs"
                    value={pcs}
                    onChange={(e) => setPcs(e.target.value)}
                    placeholder="minipc-01,minipc-02"
                  />
                </div>
              )}

              <div className="space-y-1">
                <Label htmlFor="exec-jitter">jitter (optional, humantime — e.g. 30s, 5m)</Label>
                <Input
                  id="exec-jitter"
                  value={jitter}
                  onChange={(e) => setJitter(e.target.value)}
                  placeholder=""
                />
              </div>
            </>
          )}

          <Button
            onClick={onFire}
            disabled={!jobId || !targetReady || mut.isPending}
          >
            {mut.isPending
              ? <Loader2 className="size-4 animate-spin" />
              : <Send className="size-4" />}
            POST /api/exec/{jobId || '<job_id>'}
          </Button>
        </CardContent>
      </Card>

      {mut.error && <ErrorCard title="Exec failed" error={mut.error} />}
      {mut.data && (
        <Card>
          <CardHeader>
            <CardTitle>Exec accepted</CardTitle>
            <CardDescription>
              {mut.data.target_count} target(s) · {mut.data.subjects.length} subject(s)
            </CardDescription>
          </CardHeader>
          <CardContent>
            <JsonOutput value={mut.data} />
          </CardContent>
        </Card>
      )}
    </div>
  );
}
