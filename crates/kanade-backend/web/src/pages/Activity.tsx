import { useMutation, useQuery } from '@tanstack/react-query';
import { ChevronsDown, ChevronsUp, ExternalLink, Loader2, Skull } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, formatError } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

/// v0.27.x: how many chars of stdout / stderr to show in the table
/// row's collapsed preview. The full body is fetched on demand when
/// the operator clicks "show more" (inline expand) or opens the
/// /activity/{result_id} detail page in a new tab.
const PREVIEW_CHARS = 200;

/// First N characters of a UUID-shaped identifier (result_id /
/// exec_id) shown in table cells; the full value is on the detail
/// page. 8 chars × base16 = 32 bits of selectivity — plenty for
/// disambiguating siblings in a single Activity window.
const ID_PREVIEW_LENGTH = 8;

type ResultRow = {
  /** v0.29 / Issue #19: agent-minted per-PC UUID and the detail-route
   *  identifier. Pre-v0.29 rows had result_id == request_id after the
   *  migration backfill, so old browser-cached deep links still work. */
  result_id: string;
  request_id: string;
  /** v0.29 / Issue #19: back-link to executions.exec_id. Null for
   *  ad-hoc `kanade run` rows and pre-migration rows. */
  exec_id: string | null;
  pc_id: string;
  /** v0.30 / PR α' unified: null means the row is still in flight
   *  (events.started landed but no ExecResult yet). Renders as a
   *  "running…" placeholder rather than `0` to avoid confusion with
   *  successful exit code 0. */
  exit_code: number | null;
  stdout: string;
  stderr: string;
  started_at: string | null;
  /** v0.30 / PR α' unified: null while in-flight. */
  finished_at: string | null;
  /** v0.27: surfaced from `execution_results.job_id`. Empty for
   *  ad-hoc `kanade run` rows / pre-migration-0002 rows. Drives the
   *  per-row Kill button. */
  job_id: string | null;
  /** v0.30 / PR α' unified: pinned Manifest version, populated by
   *  events.started. None for legacy rows / result-first races. */
  version: string | null;
};

// Each regex keystroke triggers a `useQuery` refetch that scans up to
// MAX_FETCH rows on the backend — debounce the inputs so a typed
// pattern doesn't spam the server while the operator is still typing.
const FILTER_DEBOUNCE_MS = 300;

function useDebouncedValue<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), delay);
    return () => clearTimeout(t);
  }, [value, delay]);
  return debounced;
}

const SINCE_PRESETS: Array<{ value: string; label: string; ms: number | null }> = [
  { value: '1h',  label: 'last 1h',   ms: 60 * 60 * 1000 },
  { value: '24h', label: 'last 24h',  ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  label: 'last 7d',   ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', label: 'last 30d',  ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', label: 'all time',  ms: null },
];

export function Activity() {
  const confirm = useConfirm();
  const [pcId, setPcId] = useState('');
  const [jobId, setJobId] = useState('');
  const [execId, setExecId] = useState('');
  const [stdoutFilter, setStdoutFilter] = useState('');
  const [stderrFilter, setStderrFilter] = useState('');
  const [status, setStatus] = useState<'' | 'running' | 'success' | 'failure'>('');
  const [since, setSince] = useState('24h');
  const [limit, setLimit] = useState(50);

  const sinceIso = useMemo(() => {
    const preset = SINCE_PRESETS.find((p) => p.value === since);
    if (!preset?.ms) return null;
    return new Date(Date.now() - preset.ms).toISOString();
  }, [since]);

  const dPcId         = useDebouncedValue(pcId,         FILTER_DEBOUNCE_MS);
  const dJobId        = useDebouncedValue(jobId,        FILTER_DEBOUNCE_MS);
  const dExecId       = useDebouncedValue(execId,       FILTER_DEBOUNCE_MS);
  const dStdoutFilter = useDebouncedValue(stdoutFilter, FILTER_DEBOUNCE_MS);
  const dStderrFilter = useDebouncedValue(stderrFilter, FILTER_DEBOUNCE_MS);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (dPcId)         sp.set('pc_id', dPcId);
    if (dJobId)        sp.set('job_id', dJobId);
    if (dExecId)       sp.set('exec_id', dExecId);
    if (dStdoutFilter) sp.set('stdout', dStdoutFilter);
    if (dStderrFilter) sp.set('stderr', dStderrFilter);
    if (status)        sp.set('status', status);
    if (sinceIso)      sp.set('since', sinceIso);
    return sp.toString();
  }, [dPcId, dJobId, dExecId, dStdoutFilter, dStderrFilter, status, sinceIso, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['results', queryString],
    queryFn: () => apiFetch<ResultRow[]>(`/api/results?${queryString}`),
  });

  // v0.27: Layer 3 kill — POST /api/jobs/{job_id}/kill. v0.29 / Issue
  // #19: the backend now SELECTs every still-running exec_id for the
  // cmd and publishes kill.{exec_id} per deployment; pre-v0.29 it
  // published kill.{cmd_id} which no agent subscribes to. So the
  // button was a no-op since v0.27 — now actually terminates running
  // children. Per-row availability is gated on the row carrying a
  // job_id (ad-hoc `kanade run` rows have None).
  //
  // Round 2 review (CodeRabbit #38): per-row pending tracked via a
  // Set<string> so a kill click on one row doesn't disable every
  // other row's kill button while the first request is inflight.
  const [pendingKill, setPendingKill] = useState<Set<string>>(new Set());
  const kill = useMutation({
    mutationFn: (job_id: string) =>
      apiFetch(`/api/jobs/${encodeURIComponent(job_id)}/kill`, { method: 'POST' }),
    onMutate: (job_id) => {
      setPendingKill((prev) => new Set(prev).add(job_id));
    },
    onSuccess: (_d, job_id) => toast.success(`Kill signal sent to ${job_id}`),
    onError: (e) => toast.error(`Kill failed: ${formatError(e)}`),
    onSettled: (_d, _e, job_id) => {
      setPendingKill((prev) => {
        const next = new Set(prev);
        next.delete(job_id);
        return next;
      });
    },
  });

  const rows = data ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">Activity</h2>
        <Badge variant="violet">{rows.length} shown{isFetching && !isLoading ? '…' : ''}</Badge>
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="res-pc">pc_id</Label>
            <Input
              id="res-pc"
              placeholder="regex — eg. ^PC001 or PC001"
              value={pcId}
              onChange={(e) => setPcId(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-job">job_id</Label>
            <Input
              id="res-job"
              placeholder="regex — eg. ^job-foo"
              value={jobId}
              onChange={(e) => setJobId(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-exec">exec_id</Label>
            <Input
              id="res-exec"
              placeholder="regex — eg. ^abc1234"
              value={execId}
              onChange={(e) => setExecId(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-stdout">stdout</Label>
            <Input
              id="res-stdout"
              placeholder="regex — eg. timeout|panic"
              value={stdoutFilter}
              onChange={(e) => setStdoutFilter(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-stderr">stderr</Label>
            <Input
              id="res-stderr"
              placeholder="regex — eg. permission denied"
              value={stderrFilter}
              onChange={(e) => setStderrFilter(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-status">status</Label>
            <Select
              id="res-status"
              value={status}
              onChange={(e) =>
                setStatus(e.target.value as '' | 'running' | 'success' | 'failure')
              }
            >
              <option value="">(any)</option>
              <option value="running">running (in flight)</option>
              <option value="success">success (exit 0)</option>
              <option value="failure">failure (exit ≠ 0)</option>
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-since">since</Label>
            <Select id="res-since" value={since} onChange={(e) => setSince(e.target.value)}>
              {SINCE_PRESETS.map((p) => (
                <option key={p.value} value={p.value}>{p.label}</option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-limit">limit</Label>
            <Select
              id="res-limit"
              value={String(limit)}
              onChange={(e) => setLimit(Number(e.target.value))}
            >
              <option value="50">50</option>
              <option value="200">200</option>
              <option value="1000">1000</option>
            </Select>
          </div>
        </CardContent>
      </Card>

      {isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />loading activity…
        </div>
      ) : error ? (
        <ErrorCard title="Couldn't load activity" error={error} />
      ) : rows.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>No activity matches</CardTitle></CardHeader>
          <CardContent className="text-muted">
            Widen the filter window or clear pc_id / job_id / exec_id / stdout / stderr / status to see older runs.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>result_id</TableHead>
              <TableHead>pc_id</TableHead>
              <TableHead>job_id</TableHead>
              <TableHead>exec_id</TableHead>
              <TableHead>exit</TableHead>
              <TableHead>started</TableHead>
              <TableHead>finished</TableHead>
              <TableHead>stdout / stderr</TableHead>
              <TableHead>actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((r) => (
              <TableRow key={r.result_id}>
                <TableCell>
                  {/* result_id (v0.29) is the detail-route key — was
                      request_id pre-v0.29 but that's no longer unique
                      across broadcast Commands. Ctrl/⌘ click opens
                      a new tab so operators can compare results
                      side-by-side. */}
                  <Link
                    to={`/activity/${encodeURIComponent(r.result_id)}`}
                    className="text-accent hover:underline inline-flex items-center gap-1"
                    title="Open detail page (Ctrl/⌘+click for new tab)"
                  >
                    <code className="text-xs">{r.result_id.slice(0, ID_PREVIEW_LENGTH)}</code>
                    <ExternalLink className="size-3" />
                  </Link>
                </TableCell>
                <TableCell><code className="text-xs">{r.pc_id}</code></TableCell>
                <TableCell>
                  {r.job_id
                    ? <code className="text-xs">{r.job_id}</code>
                    : <span className="text-muted text-xs">—</span>}
                </TableCell>
                <TableCell>
                  {r.exec_id
                    ? <code className="text-xs">{r.exec_id.slice(0, ID_PREVIEW_LENGTH)}</code>
                    : <span className="text-muted text-xs">—</span>}
                </TableCell>
                <TableCell>
                  {/* v0.30 / PR α' unified: exit_code is null while
                      the row is in flight (events.started landed
                      but no ExecResult yet). Show a 'running' badge
                      instead of `0` or empty so operators see
                      lifecycle clearly. */}
                  {r.exit_code === null ? (
                    <Badge variant="violet">running</Badge>
                  ) : (
                    <Badge variant={r.exit_code === 0 ? 'success' : 'danger'}>
                      {r.exit_code}
                    </Badge>
                  )}
                </TableCell>
                <TableCell className="text-muted text-xs">{fmtIsoLocal(r.started_at)}</TableCell>
                <TableCell className="text-muted text-xs">
                  {/* v0.30 / PR α' unified: finished_at null = still
                      running. fmtIsoLocal returns "—" for null
                      which is OK but ambiguous with "no data"; show
                      "running…" explicitly. */}
                  {r.finished_at ? fmtIsoLocal(r.finished_at) : 'running…'}
                </TableCell>
                <TableCell className="max-w-md">
                  <StdioPreview resultId={r.result_id} stdout={r.stdout} stderr={r.stderr} />
                </TableCell>
                <TableCell>
                  {/* v0.30 / PR α' unified: Activity now lists
                      in-flight rows too (events.started inserts
                      with finished_at = NULL). The condition
                      `r.job_id && !r.finished_at` was historically
                      always false because every Activity row had
                      a finished_at; now it actually fires for
                      running rows. */}
                  {r.job_id && !r.finished_at ? (
                    <Button
                      variant="danger"
                      size="sm"
                      disabled={pendingKill.has(r.job_id)}
                      onClick={async () => {
                        const ok = await confirm({
                          title: `Kill this run (${r.job_id})?`,
                          description:
                            'The agent will terminate the running child process. The run will be recorded as killed.',
                          confirmLabel: 'Kill',
                          danger: true,
                        });
                        if (ok) kill.mutate(r.job_id!);
                      }}
                      title="Tell the agent to terminate this job's child process"
                    >
                      <Skull className="size-3.5" />
                      kill
                    </Button>
                  ) : (
                    <span className="text-muted text-xs">—</span>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
      {kill.error && <ErrorCard title="Kill failed" error={kill.error} />}
    </div>
  );
}

/**
 * Stdout / stderr preview cell with on-demand inline expansion.
 *
 * Default state: shows the first PREVIEW_CHARS chars of each stream,
 * matching the historical table render. When either stream is
 * actually longer than the preview cutoff, a "show more" button
 * appears that toggles to a full-body view fetched lazily via
 * GET /api/results/{result_id} (v0.29 — was /api/results/{request_id}
 * pre-v0.29). The detail page (linked via the result_id column on
 * the same row) is the heavier alternative — dedicated tab, copy
 * buttons, header chrome.
 *
 * The expanded body is bounded by max-h-96 + overflow-y-auto so an
 * enormous payload doesn't push every other row off-screen; the
 * detail page in a new tab is the right home for that case.
 */
function StdioPreview({
  resultId,
  stdout,
  stderr,
}: {
  resultId: string;
  stdout: string;
  stderr: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const couldExpand = stdout.length > PREVIEW_CHARS || stderr.length > PREVIEW_CHARS;
  const { data, isFetching, error } = useQuery({
    queryKey: ['result', resultId],
    queryFn: () =>
      apiFetch<{ stdout: string; stderr: string }>(
        `/api/results/${encodeURIComponent(resultId)}`,
      ),
    enabled: expanded,
  });

  const stdoutBody = expanded && data ? data.stdout : stdout.slice(0, PREVIEW_CHARS);
  const stderrBody = expanded && data ? data.stderr : stderr.slice(0, PREVIEW_CHARS);

  return (
    <div>
      <pre
        className={
          expanded
            ? 'text-xs whitespace-pre-wrap break-words bg-muted/5 p-2 rounded max-h-96 overflow-y-auto'
            : 'text-xs whitespace-pre-wrap break-words bg-muted/5 p-2 rounded'
        }
      >
        {stdoutBody || '(empty)'}
      </pre>
      {(expanded ? stderrBody : stderr) && (
        <pre
          className={
            expanded
              ? 'text-xs whitespace-pre-wrap break-words text-danger bg-danger/5 p-2 rounded mt-1 max-h-96 overflow-y-auto'
              : 'text-xs whitespace-pre-wrap break-words text-danger bg-danger/5 p-2 rounded mt-1'
          }
        >
          {stderrBody}
        </pre>
      )}
      {couldExpand && (
        <button
          type="button"
          className="text-xs text-muted hover:text-fg inline-flex items-center gap-1 mt-1"
          onClick={() => setExpanded((v) => !v)}
        >
          {isFetching ? (
            <>
              <Loader2 className="size-3 animate-spin" />
              loading…
            </>
          ) : expanded ? (
            <>
              <ChevronsUp className="size-3" />
              collapse
            </>
          ) : (
            <>
              <ChevronsDown className="size-3" />
              show more
            </>
          )}
        </button>
      )}
      {error && <span className="text-xs text-danger">fetch failed: {String(error)}</span>}
    </div>
  );
}
