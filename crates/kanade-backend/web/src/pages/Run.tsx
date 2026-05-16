import { useMutation } from '@tanstack/react-query';
import { Loader2, Play } from 'lucide-react';
import { useState } from 'react';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Textarea } from '@/components/ui/textarea';
import { apiFetch } from '@/lib/api';
import type { ExecResult } from '@/lib/types';

type RunBody = {
  pc_id: string;
  shell: string;
  script: string;
  timeout_secs: number;
  job_id?: string;
};

export function Run() {
  const [pcId, setPcId] = useState('');
  const [shell, setShell] = useState('powershell');
  const [timeout, setTimeout] = useState(60);
  const [jobId, setJobId] = useState('');
  const [script, setScript] = useState('');

  const mut = useMutation({
    mutationFn: (body: RunBody) =>
      apiFetch<ExecResult>('/api/run', {
        method: 'POST',
        body: JSON.stringify(body),
      }),
  });

  const onSubmit = () => {
    if (!pcId.trim() || !script.trim()) return;
    const body: RunBody = {
      pc_id: pcId.trim(),
      shell,
      script,
      timeout_secs: timeout,
    };
    if (jobId.trim()) body.job_id = jobId.trim();
    mut.mutate(body);
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>Run a script on one PC</CardTitle>
          <CardDescription>
            Posts to <code>/api/run</code>; backend forwards over NATS and waits for the agent's
            <code> ExecResult</code>. Same surface as <code>kanade run &lt;pc_id&gt; -- &lt;script&gt;</code>.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-1 md:grid-cols-4 gap-3">
            <div>
              <Label>pc_id</Label>
              <Input value={pcId} onChange={(e) => setPcId(e.target.value)} placeholder="MINIPC-01" />
            </div>
            <div>
              <Label>shell</Label>
              <Select value={shell} onChange={(e) => setShell(e.target.value)}>
                <option value="powershell">powershell</option>
                <option value="cmd">cmd</option>
              </Select>
            </div>
            <div>
              <Label>timeout (s)</Label>
              <Input
                type="number"
                min={1}
                value={timeout}
                onChange={(e) => setTimeout(parseInt(e.target.value, 10) || 60)}
              />
            </div>
            <div>
              <Label>job_id (optional)</Label>
              <Input value={jobId} onChange={(e) => setJobId(e.target.value)} placeholder="ad-hoc-…" />
            </div>
          </div>
          <div>
            <Label>script</Label>
            <Textarea
              value={script}
              onChange={(e) => setScript(e.target.value)}
              placeholder="echo hello from kanade web"
              className="min-h-32"
            />
          </div>
          <Button onClick={onSubmit} disabled={!pcId.trim() || !script.trim() || mut.isPending}>
            {mut.isPending ? <Loader2 className="size-4 animate-spin" /> : <Play className="size-4" />}
            POST /api/run
          </Button>
        </CardContent>
      </Card>

      {mut.error && <ErrorCard title="Run failed" error={mut.error} />}
      {mut.data && (
        <Card>
          <CardHeader>
            <CardTitle>
              exit_code: <span className={mut.data.exit_code === 0 ? 'text-success' : 'text-danger'}>{mut.data.exit_code}</span>
            </CardTitle>
            <CardDescription>
              {mut.data.pc_id} · {mut.data.started_at} → {mut.data.finished_at}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <div>
              <Label>stdout</Label>
              <JsonOutput value={mut.data.stdout || '(empty)'} />
            </div>
            {mut.data.stderr && (
              <div>
                <Label>stderr</Label>
                <JsonOutput value={mut.data.stderr} className="text-danger" />
              </div>
            )}
          </CardContent>
        </Card>
      )}
    </div>
  );
}
