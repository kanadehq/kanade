import { useQuery } from '@tanstack/react-query';
import { ArrowLeft, Check, Copy, Loader2, Radio } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useParams } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Select } from '@/components/ui/select';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

/** Reply shape from `GET /api/results/{result_id}/tail`. */
type TailResponse = {
  running: boolean;
  live: boolean;
  stdout: string;
  stderr: string;
  stdout_truncated: boolean;
  stderr_truncated: boolean;
  exit_code: number | null;
};

const LIVE_POLL_MS = 5_000;
const LIVE_TAIL_KB = 128;

type ResultDetailRow = {
  result_id: string;
  request_id: string;
  exec_id: string | null;
  pc_id: string;
  /** v0.30 / PR α' unified: null while in-flight. */
  exit_code: number | null;
  stdout: string;
  stderr: string;
  started_at: string | null;
  /** v0.30 / PR α' unified: null while in-flight. */
  finished_at: string | null;
  job_id: string | null;
  /** v0.30 / PR α' unified: pinned manifest version. */
  version: string | null;
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
  const { t } = useTranslation('result-detail');
  const { resultId } = useParams<{ resultId: string }>();
  // Live tail is strictly opt-in: OFF by default, so opening this page
  // behaves exactly as before — a single metadata fetch, no polling,
  // and zero `/tail` requests. Flipping the toggle ON (in LiveConsole)
  // is the only thing that starts any recurring network traffic.
  const [live, setLive] = useState(false);
  const { data, error, isLoading } = useQuery({
    queryKey: ['result', resultId],
    queryFn: () => apiFetch<ResultDetailRow>(`/api/results/${encodeURIComponent(resultId!)}`),
    enabled: !!resultId,
    // Only poll the metadata while live tailing is ON and the row is
    // still in-flight — so the page flips to the final (full,
    // untruncated) DB output the moment the ExecResult projects. With
    // the toggle OFF this is `false`, i.e. no background polling at all.
    refetchInterval: (q) =>
      live && q.state.data && q.state.data.finished_at === null ? LIVE_POLL_MS : false,
  });

  const inFlight = !!data && data.finished_at === null;

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />
        {t('loading')}
      </div>
    );
  }
  if (error) return <ErrorCard title={t('errors.loadFailed')} error={error} />;
  if (!data) return <ErrorCard title={t('errors.notFound')} error={new Error(`${resultId}`)} />;

  return (
    <div className="space-y-4">
      <div className="flex items-baseline gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/activity">
            <ArrowLeft className="size-3.5" />
            {t('backToActivity')}
          </Link>
        </Button>
        <h2 className="text-xl">
          <code className="text-base">{data.result_id}</code>
        </h2>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">{t('metadata.title')}</CardTitle>
        </CardHeader>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 gap-y-2 gap-x-6 text-sm">
          <Field label={t('fields.pcId')} value={<code>{data.pc_id}</code>} />
          <Field
            label={t('fields.exitCode')}
            value={
              data.exit_code === null ? (
                <Badge variant="violet">{t('values.running')}</Badge>
              ) : (
                <Badge variant={data.exit_code === 0 ? 'success' : 'danger'}>
                  {data.exit_code}
                </Badge>
              )
            }
          />
          <Field
            label={t('fields.jobId')}
            value={
              data.job_id ? (
                <code>{data.job_id}</code>
              ) : (
                <span className="text-muted text-xs">{t('values.noJob')}</span>
              )
            }
          />
          <Field
            label={t('fields.execId')}
            value={
              data.exec_id ? (
                <code className="text-xs">{data.exec_id}</code>
              ) : (
                <span className="text-muted text-xs">{t('values.noExecId')}</span>
              )
            }
          />
          <Field label={t('fields.requestId')} value={<code className="text-xs">{data.request_id}</code>} />
          <Field label={t('fields.resultId')} value={<code className="text-xs">{data.result_id}</code>} />
          <Field
            label={t('fields.version')}
            value={
              data.version ? (
                <code className="text-xs">{data.version}</code>
              ) : (
                <span className="text-muted text-xs">{t('values.noVersion')}</span>
              )
            }
          />
          <Field label={t('fields.startedAt')} value={<span className="text-muted text-xs">{fmtIsoLocal(data.started_at)}</span>} />
          <Field
            label={t('fields.finishedAt')}
            value={
              <span className="text-muted text-xs">
                {data.finished_at ? fmtIsoLocal(data.finished_at) : t('values.runningEllipsis')}
              </span>
            }
          />
        </CardContent>
      </Card>

      {inFlight ? (
        <LiveConsole resultId={data.result_id} live={live} setLive={setLive} />
      ) : (
        <>
          <StreamPane title={t('stream.stdout')} body={data.stdout} emptyHint={t('stream.emptyStdoutHint')} />
          <StreamPane title={t('stream.stderr')} body={data.stderr} emptyHint={t('stream.emptyStderrHint')} danger />
        </>
      )}
    </div>
  );
}

/**
 * Live stdout/stderr console for an in-flight job. Polls
 * `GET /api/results/{id}/tail` every {@link LIVE_POLL_MS} ms (same UX
 * as the agent-log auto-refresh) while the operator leaves it enabled.
 *
 * The backend returns the agent's in-memory ring-buffer tail while the
 * job runs, then `running: false` once it exits. We keep polling the
 * tail through the brief grace window; meanwhile the parent's metadata
 * query (which also polls while in-flight) picks up `finished_at` and
 * swaps this console out for the full, untruncated DB output. So the
 * truncated 128 KB live view is only ever shown transiently — the
 * authoritative final output always replaces it.
 */
function LiveConsole({
  resultId,
  live,
  setLive,
}: {
  resultId: string;
  live: boolean;
  setLive: (v: boolean) => void;
}) {
  const { t } = useTranslation('result-detail');
  const stdoutRef = useRef<HTMLPreElement>(null);
  const stderrRef = useRef<HTMLPreElement>(null);
  // Follow-mode scroll: only snap a pane to the bottom when the reader
  // is already pinned there. Scrolling up to inspect earlier output
  // must not be yanked back down by the next 5s poll.
  const stdoutPinned = useRef(true);
  const stderrPinned = useRef(true);
  const onScroll = (el: HTMLPreElement | null, pinned: React.MutableRefObject<boolean>) => {
    if (!el) return;
    pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  };

  const tailQ = useQuery({
    queryKey: ['result-tail', resultId],
    queryFn: () => apiFetch<TailResponse>(`/api/results/${encodeURIComponent(resultId)}/tail`),
    // Nothing fires until the operator flips the toggle ON. Once the
    // agent reports the job finished we also stop polling — the parent
    // metadata poll takes over and swaps in the final output.
    enabled: live,
    refetchInterval: (q) => (live && q.state.data?.running !== false ? LIVE_POLL_MS : false),
  });

  const tail = live ? tailQ.data : undefined;

  // Auto-scroll each pane to the newest output on update — but only
  // when the reader is still pinned to the bottom (follow mode).
  useEffect(() => {
    if (stdoutRef.current && stdoutPinned.current)
      stdoutRef.current.scrollTop = stdoutRef.current.scrollHeight;
    if (stderrRef.current && stderrPinned.current)
      stderrRef.current.scrollTop = stderrRef.current.scrollHeight;
  }, [tail?.stdout, tail?.stderr]);

  const finishing = tail?.running === false;

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0">
        <CardTitle className="text-sm flex items-center gap-2">
          <Radio
            className={
              live && !finishing ? 'size-4 text-accent animate-pulse' : 'size-4 text-muted'
            }
          />
          {t('live.toggleLabel')}
          {tailQ.isFetching && <Loader2 className="size-3 animate-spin text-muted" />}
        </CardTitle>
        <div className="flex items-center gap-2">
          <Select
            value={live ? 'on' : 'off'}
            onChange={(e) => setLive(e.target.value === 'on')}
            aria-label={t('live.toggleLabel')}
          >
            <option value="off">{t('live.off')}</option>
            <option value="on">{t('live.on')}</option>
          </Select>
        </div>
      </CardHeader>
      <CardContent className="space-y-2">
        {!live && (
          <p className="text-muted text-xs">{t('live.offHint')}</p>
        )}
        {finishing && (
          <div className="flex items-center gap-2 text-muted text-xs">
            <Loader2 className="size-3 animate-spin" />
            {t('live.finishing')}
          </div>
        )}
        {tailQ.error && (
          <span className="text-xs text-danger">
            {t('live.error', { error: String(tailQ.error) })}
          </span>
        )}
        {live && (tailQ.isLoading ? (
          // First fetch after the toggle flips on: show a spinner, not
          // the empty-output panes (tail is still undefined here).
          <div className="flex items-center gap-2 text-muted text-xs">
            <Loader2 className="size-3 animate-spin" />
            {t('live.waiting')}
          </div>
        ) : tail && !tail.live && tail.running ? (
          <div className="text-muted text-xs space-y-1">
            <div className="flex items-center gap-2">
              <Loader2 className="size-3 animate-spin" />
              {t('live.waiting')}
            </div>
            <p>{t('live.waitingHint')}</p>
          </div>
        ) : (
          <>
            <div className="space-y-1">
              <div className="text-muted text-xs uppercase tracking-wide flex items-center gap-2">
                {t('live.stdout')}
                {tail?.stdout_truncated && (
                  <span className="text-amber normal-case tracking-normal">
                    {t('live.truncated', { kb: LIVE_TAIL_KB })}
                  </span>
                )}
              </div>
              <pre
                ref={stdoutRef}
                onScroll={() => onScroll(stdoutRef.current, stdoutPinned)}
                className="text-xs whitespace-pre-wrap break-words bg-muted/5 p-3 rounded max-h-[40vh] overflow-y-auto font-mono"
              >
                {tail?.stdout || t('live.empty')}
              </pre>
            </div>
            <div className="space-y-1">
              <div className="text-muted text-xs uppercase tracking-wide flex items-center gap-2">
                {t('live.stderr')}
                {tail?.stderr_truncated && (
                  <span className="text-amber normal-case tracking-normal">
                    {t('live.truncated', { kb: LIVE_TAIL_KB })}
                  </span>
                )}
              </div>
              <pre
                ref={stderrRef}
                onScroll={() => onScroll(stderrRef.current, stderrPinned)}
                className="text-xs whitespace-pre-wrap break-words text-danger bg-danger/5 p-3 rounded max-h-[40vh] overflow-y-auto font-mono"
              >
                {tail?.stderr || t('live.empty')}
              </pre>
            </div>
          </>
        ))}
      </CardContent>
    </Card>
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
  const { t } = useTranslation('result-detail');
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
            ({empty ? t('stream.empty') : t('stream.char', { count: body.length })})
          </span>
        </CardTitle>
        <Button
          variant="ghost"
          size="sm"
          onClick={onCopy}
          disabled={empty}
          title={empty ? t('stream.nothingToCopy') : t('stream.copyTitle')}
        >
          {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
          {copied ? t('stream.copied') : t('stream.copy')}
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
