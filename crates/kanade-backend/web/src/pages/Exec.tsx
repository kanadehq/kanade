import { useMutation, useQuery } from '@tanstack/react-query';
import { Loader2, Send } from 'lucide-react';
import { useState } from 'react';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
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

export function Exec() {
  const [jobId, setJobId] = useState('');

  const jobsQ = useQuery({
    queryKey: ['jobs'],
    queryFn: () => apiFetch<JobRow[]>('/api/jobs'),
  });

  const mut = useMutation({
    mutationFn: (id: string) =>
      apiFetch<ExecResponse>(`/api/exec/${encodeURIComponent(id)}`, { method: 'POST' }),
  });

  const jobs = jobsQ.data ?? [];

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>Exec a registered job</CardTitle>
          <CardDescription>
            Posts to <code>/api/exec/&lt;job_id&gt;</code>. The backend resolves
            the job from <code>BUCKET_JOBS</code> and fans the Command out at
            its declared targets. Register new jobs with{' '}
            <code>kanade job create &lt;manifest.yaml&gt;</code>; manage them on
            the <a href="/jobs" className="underline">Jobs</a> page.
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
          )}
          <Button
            onClick={() => jobId && mut.mutate(jobId)}
            disabled={!jobId || mut.isPending}
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
