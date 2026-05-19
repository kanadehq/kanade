import { useMutation, useQuery } from '@tanstack/react-query';
import { ChevronsDown, ChevronsUp, ExternalLink, Loader2, Skull } from 'lucide-react';
import { useMemo, useState } from 'react';
import { Link } from 'react-router-dom';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch } from '@/lib/api';
import { fmtIsoLocal } from '@/lib/utils';

/// v0.27.x: how many chars of stdout / stderr to show in the table
/// row's collapsed preview. The full body is fetched on demand when
/// the operator clicks "show more" (inline expand) or opens the
/// /results/{request_id} detail page in a new tab.
const PREVIEW_CHARS = 200;

type ResultRow = {
  request_id: string;
  pc_id: string;
  exit_code: number;
  stdout: string;
  stderr: string;
  started_at: string | null;
  finished_at: string | null;
  /** v0.27: surfaced from `execution_results.job_id`. Empty for
   *  ad-hoc `kanade run` rows / pre-migration-0002 rows. Drives the
   *  per-row Kill button. */
  job_id: string | null;
};

const SINCE_PRESETS: Array<{ value: string; label: string; ms: number | null }> = [
  { value: '1h',  label: 'last 1h',   ms: 60 * 60 * 1000 },
  { value: '24h', label: 'last 24h',  ms: 24 * 60 * 60 * 1000 },
  { value: '7d',  label: 'last 7d',   ms: 7 * 24 * 60 * 60 * 1000 },
  { value: '30d', label: 'last 30d',  ms: 30 * 24 * 60 * 60 * 1000 },
  { value: 'all', label: 'all time',  ms: null },
];

export function Results() {
  const [pcId, setPcId] = useState('');
  const [status, setStatus] = useState<'' | 'success' | 'failure'>('');
  const [since, setSince] = useState('24h');
  const [limit, setLimit] = useState(50);

  const sinceIso = useMemo(() => {
    const preset = SINCE_PRESETS.find((p) => p.value === since);
    if (!preset?.ms) return null;
    return new Date(Date.now() - preset.ms).toISOString();
  }, [since]);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (pcId)     sp.set('pc_id', pcId);
    if (status)   sp.set('status', status);
    if (sinceIso) sp.set('since', sinceIso);
    return sp.toString();
  }, [pcId, status, sinceIso, limit]);

  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['results', queryString],
    queryFn: () => apiFetch<ResultRow[]>(`/api/results?${queryString}`),
  });

  // v0.27: Layer 3 kill — publishes on `kill.{job_id}` so any agent
  // currently running this job's child process terminates it. Only
  // useful when the job is *currently running*; killing a completed
  // execution is a no-op (no subscriber). Per-row availability is
  // gated on the row carrying a job_id (ad-hoc `kanade run` rows
  // have None).
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
        <h2 className="text-xl">Recent results</h2>
        <Badge variant="violet">{rows.length} shown{isFetching && !isLoading ? '…' : ''}</Badge>
      </div>

      <Card>
        <CardContent className="grid grid-cols-1 sm:grid-cols-4 gap-3 p-4">
          <div className="space-y-1">
            <Label htmlFor="res-pc">pc_id</Label>
            <Input
              id="res-pc"
              placeholder="exact match"
              value={pcId}
              onChange={(e) => setPcId(e.target.value)}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor="res-status">status</Label>
            <Select
              id="res-status"
              value={status}
              onChange={(e) => setStatus(e.target.value as '' | 'success' | 'failure')}
            >
              <option value="">(any)</option>
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
          <Loader2 className="size-4 animate-spin" />loading results…
        </div>
      ) : error ? (
        <ErrorCard title="Couldn't load results" error={error} />
      ) : rows.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>No results match</CardTitle></CardHeader>
          <CardContent className="text-muted">
            Widen the filter window or clear pc_id / status to see older runs.
          </CardContent>
        </Card>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>request_id</TableHead>
              <TableHead>pc_id</TableHead>
              <TableHead>job_id</TableHead>
              <TableHead>exit</TableHead>
              <TableHead>started</TableHead>
              <TableHead>finished</TableHead>
              <TableHead>stdout / stderr</TableHead>
              <TableHead>actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((r) => (
              <TableRow key={r.request_id}>
                <TableCell>
                  {/* request_id is a Link to the detail page so
                      Ctrl/⌘ click opens a new tab — operators can
                      compare multiple results side-by-side via
                      browser tabs / window split. Plain click stays
                      on the same tab. */}
                  <Link
                    to={`/results/${encodeURIComponent(r.request_id)}`}
                    className="text-accent hover:underline inline-flex items-center gap-1"
                    title="Open detail page (Ctrl/⌘+click for new tab)"
                  >
                    <code className="text-xs">{r.request_id.slice(0, 8)}</code>
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
                  <Badge variant={r.exit_code === 0 ? 'success' : 'danger'}>{r.exit_code}</Badge>
                </TableCell>
                <TableCell className="text-muted text-xs">{fmtIsoLocal(r.started_at)}</TableCell>
                <TableCell className="text-muted text-xs">{fmtIsoLocal(r.finished_at)}</TableCell>
                <TableCell className="max-w-md">
                  <StdioPreview requestId={r.request_id} stdout={r.stdout} stderr={r.stderr} />
                </TableCell>
                <TableCell>
                  {r.job_id ? (
                    <Button
                      variant="danger"
                      size="sm"
                      disabled={r.job_id ? pendingKill.has(r.job_id) : false}
                      onClick={() => {
                        if (
                          window.confirm(
                            `Publish kill.${r.job_id}?\n\n` +
                              `Any agent currently running this job's child process will terminate it. No effect if the run already finished.`,
                          )
                        )
                          kill.mutate(r.job_id!);
                      }}
                      title="Publish kill.{job_id} (Layer 3)"
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
 * GET /api/results/{request_id}. The detail page (linked via the
 * request_id column on the same row) is the heavier alternative —
 * dedicated tab, copy buttons, header chrome.
 *
 * The expanded body is bounded by max-h-96 + overflow-y-auto so an
 * enormous payload doesn't push every other row off-screen; the
 * detail page in a new tab is the right home for that case.
 */
function StdioPreview({
  requestId,
  stdout,
  stderr,
}: {
  requestId: string;
  stdout: string;
  stderr: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const couldExpand = stdout.length > PREVIEW_CHARS || stderr.length > PREVIEW_CHARS;
  const { data, isFetching, error } = useQuery({
    queryKey: ['result', requestId],
    queryFn: () =>
      apiFetch<{ stdout: string; stderr: string }>(
        `/api/results/${encodeURIComponent(requestId)}`,
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
