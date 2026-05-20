import { useQuery } from '@tanstack/react-query';
import { ArrowLeft, Check, Copy, Loader2 } from 'lucide-react';
import { useState } from 'react';
import { Link, useParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

type ResultDetailRow = {
  result_id: string;
  request_id: string;
  exec_id: string | null;
  pc_id: string;
  exit_code: number;
  stdout: string;
  stderr: string;
  started_at: string | null;
  finished_at: string | null;
  job_id: string | null;
};

/**
 * Per-result detail page (`/activity/{result_id}`). Route param renamed
 * from `requestId` to `resultId` in v0.29 / Issue #19 once result_id
 * became the PK — pre-v0.29 deep links keep resolving because the
 * migration backfilled result_id = request_id for legacy rows.
 *
 * Pairs with the Activity list table — operators open this in a new
 * tab via Ctrl/⌘ click on the result_id Link to compare stdout/stderr
 * across PCs side-by-side using browser tabs / window split.
 *
 * Renders the full `ExecResult` payload from `GET /api/results/{id}`
 * (no `slice` truncation, unlike the table preview). Includes
 * copy-to-clipboard buttons for stdout/stderr so the operator can
 * paste full output into an incident chat without manually selecting.
 */
export function ResultDetail() {
  const { resultId } = useParams<{ resultId: string }>();
  const { data, error, isLoading } = useQuery({
    queryKey: ['result', resultId],
    queryFn: () => apiFetch<ResultDetailRow>(`/api/results/${encodeURIComponent(resultId!)}`),
    enabled: !!resultId,
  });

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />
        loading result…
      </div>
    );
  }
  if (error) return <ErrorCard title="Couldn't load result" error={error} />;
  if (!data) return <ErrorCard title="Result not found" error={new Error(`${resultId}`)} />;

  return (
    <div className="space-y-4">
      <div className="flex items-baseline gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/activity">
            <ArrowLeft className="size-3.5" />
            back to activity
          </Link>
        </Button>
        <h2 className="text-xl">
          <code className="text-base">{data.result_id}</code>
        </h2>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">Metadata</CardTitle>
        </CardHeader>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-y-2 gap-x-6 text-sm">
          <Field label="pc_id" value={<code>{data.pc_id}</code>} />
          <Field
            label="exit_code"
            value={<Badge variant={data.exit_code === 0 ? 'success' : 'danger'}>{data.exit_code}</Badge>}
          />
          <Field
            label="job_id"
            value={
              data.job_id ? (
                <code>{data.job_id}</code>
              ) : (
                <span className="text-muted text-xs">— (ad-hoc run, no Job)</span>
              )
            }
          />
          <Field
            label="exec_id"
            value={
              data.exec_id ? (
                <code className="text-xs">{data.exec_id}</code>
              ) : (
                <span className="text-muted text-xs">— (ad-hoc / pre-v0.29 row)</span>
              )
            }
          />
          <Field label="request_id" value={<code className="text-xs">{data.request_id}</code>} />
          <Field label="result_id" value={<code className="text-xs">{data.result_id}</code>} />
          <Field label="started_at" value={<span className="text-muted text-xs">{fmtIsoLocal(data.started_at)}</span>} />
          <Field label="finished_at" value={<span className="text-muted text-xs">{fmtIsoLocal(data.finished_at)}</span>} />
        </CardContent>
      </Card>

      <StreamPane title="stdout" body={data.stdout} emptyHint="(no stdout)" />
      <StreamPane title="stderr" body={data.stderr} emptyHint="(no stderr)" danger />
    </div>
  );
}

function Field({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex gap-3 items-baseline">
      <span className="text-muted text-xs uppercase tracking-wide w-24 shrink-0">{label}</span>
      <span>{value}</span>
    </div>
  );
}

function StreamPane({
  title,
  body,
  emptyHint,
  danger,
}: {
  title: string;
  body: string;
  emptyHint: string;
  danger?: boolean;
}) {
  const [copied, setCopied] = useState(false);
  const empty = body.length === 0;

  async function onCopy() {
    try {
      await navigator.clipboard.writeText(body);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard API can fail on insecure origin or when permission
      // is denied; degrade silently rather than alert-spamming the
      // operator.
    }
  }

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0">
        <CardTitle className="text-sm flex items-center gap-2">
          {title}
          <span className="text-muted text-xs font-normal">
            ({empty ? 'empty' : `${body.length} char${body.length === 1 ? '' : 's'}`})
          </span>
        </CardTitle>
        <Button
          variant="ghost"
          size="sm"
          onClick={onCopy}
          disabled={empty}
          title={empty ? 'Nothing to copy' : 'Copy full body to clipboard'}
        >
          {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
          {copied ? 'copied' : 'copy'}
        </Button>
      </CardHeader>
      <CardContent>
        {empty ? (
          <span className="text-muted text-xs">{emptyHint}</span>
        ) : (
          <pre
            className={
              danger
                ? 'text-xs whitespace-pre-wrap break-words text-danger bg-danger/5 p-3 rounded max-h-[60vh] overflow-y-auto'
                : 'text-xs whitespace-pre-wrap break-words bg-muted/5 p-3 rounded max-h-[60vh] overflow-y-auto'
            }
          >
            {body}
          </pre>
        )}
      </CardContent>
    </Card>
  );
}
