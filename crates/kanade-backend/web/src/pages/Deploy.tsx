import { useMutation } from '@tanstack/react-query';
import { Loader2, Send } from 'lucide-react';
import { useState } from 'react';

import { ErrorCard } from '@/components/ErrorCard';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { JsonOutput } from '@/components/ui/json-output';
import { Label } from '@/components/ui/label';
import { Textarea } from '@/components/ui/textarea';
import { apiFetch } from '@/lib/api';

type DeployResponse = {
  deploy_id: string;
  job_id: string;
  version: string;
  target_count: number;
  subjects: string[];
};

export function Deploy() {
  const [json, setJson] = useState('');

  const mut = useMutation({
    mutationFn: (body: unknown) =>
      apiFetch<DeployResponse>('/api/deploy', {
        method: 'POST',
        body: JSON.stringify(body),
      }),
  });

  const onSubmit = () => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(json);
    } catch (e) {
      mut.reset();
      window.alert(`Body must be JSON. Use the CLI for YAML: kanade deploy <file.yaml>.\n\n${(e as Error).message}`);
      return;
    }
    mut.mutate(parsed);
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>Deploy a manifest</CardTitle>
          <CardDescription>
            Posts to <code>/api/deploy</code> using the JSON-equivalent of the manifest schema. For
            full YAML support, use <code>kanade deploy &lt;file.yaml&gt;</code> from the CLI — a
            browser-side YAML parser is on the backlog.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div>
            <Label>manifest (JSON)</Label>
            <Textarea
              value={json}
              onChange={(e) => setJson(e.target.value)}
              className="min-h-64"
              placeholder='{"id":"echo-test","version":"1.0.0","target":{"pcs":["MINIPC-01"]},"execute":{"shell":"powershell","script":"echo hello","timeout":"30s"}}'
            />
          </div>
          <Button onClick={onSubmit} disabled={!json.trim() || mut.isPending}>
            {mut.isPending ? <Loader2 className="size-4 animate-spin" /> : <Send className="size-4" />}
            POST /api/deploy
          </Button>
        </CardContent>
      </Card>

      {mut.error && <ErrorCard title="Deploy failed" error={mut.error} />}
      {mut.data && (
        <Card>
          <CardHeader>
            <CardTitle>Deploy accepted</CardTitle>
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
