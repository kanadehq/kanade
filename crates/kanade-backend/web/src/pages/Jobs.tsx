import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Ban,
  ChevronDown,
  ChevronRight,
  CircleCheck,
  FilePlus2,
  Hourglass,
  Loader2,
  Pencil,
  Play,
  ScrollText,
  Search,
  Send,
  Skull,
  Tags,
  Trash2,
  X,
} from 'lucide-react';
import { Fragment, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { Link } from 'react-router-dom';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { type EditorMode, YamlEditorDialog } from '@/components/YamlEditorDialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { DetailItem, DetailList } from '@/components/ui/detail-list';
import { Input } from '@/components/ui/input';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, formatError } from '@/lib/api';

type JobRow = {
  id: string;
  version: string;
  description: string | null;
  execute: {
    shell: 'powershell' | 'cmd';
    timeout: string;
    run_as?: 'system' | 'user' | 'system_gui';
    cwd?: string | null;
  };
  inventory: unknown | null;
  /** Free-form operator taxonomy (manifest `tags:`). Absent / empty
   *  for the majority of jobs — drives the tag-filter chips + search
   *  on this page, orthogonal to the id-prefix grouping. */
  tags?: string[];
  /** v0.30 / PR γ: in-flight counters joined onto each row by the
   *  backend so the Jobs page can show "is anything running right
   *  now" — drives the per-row live chip + kill button enable
   *  state. Zeros when no execution rows exist for this cmd. */
  live: {
    running: number;
    pending: number;
  };
};

export function Jobs() {
  const { t } = useTranslation('jobs');
  const qc = useQueryClient();
  // v0.34.1 (#117) wired in ConfirmDialogProvider but Jobs.tsx
  // only added the import — the hook call itself was missing, so
  // `confirm(...)` in the kill / revoke / delete handlers below
  // was resolving against `window.confirm` (which takes a string
  // and ignored the ConfirmOptions object). Adding the hook here
  // restores the intended Promise-based modal flow.
  const confirm = useConfirm();
  const { data, error, isLoading } = useQuery({
    queryKey: ['jobs'],
    queryFn: () => apiFetch<JobRow[]>('/api/jobs'),
  });

  // Master-detail split (#374): the table used to carry all 11 job
  // fields as columns, which forced IDs to wrap over three lines and
  // truncated every Windows cwd. The list now keeps the four columns
  // an operator scans for (id+description / status / live / actions)
  // and the rest moved into a right-edge Sheet opened by clicking the
  // row. `selectedId` stores the id, not the row object, so the
  // drawer re-derives fresh data from the query cache on every
  // refetch instead of pinning a stale snapshot.
  const [selectedId, setSelectedId] = useState<string | null>(null);

  // Job-catalog organisation (this PR): a free-text search, a set of
  // active tag filters (OR semantics — a row matches if it carries any
  // selected tag), and the set of collapsed id-prefix groups. All three
  // are pure view state; none touch the server.
  const [search, setSearch] = useState('');
  const [activeTags, setActiveTags] = useState<Set<string>>(new Set());
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  function toggleTag(tag: string) {
    setActiveTags((prev) => {
      const next = new Set(prev);
      if (next.has(tag)) next.delete(tag);
      else next.add(tag);
      return next;
    });
  }
  function toggleGroup(key: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  const del = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/jobs/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['jobs'] });
      qc.invalidateQueries({ queryKey: ['scripts-status'] });
      // Deleting the job the drawer is showing leaves it pointing at
      // a row that no longer exists — close it instead of rendering
      // an empty shell after the refetch lands.
      setSelectedId((prev) => (prev === id ? null : prev));
      toast.success(t('toast.deleteSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.deleteFailure', { error: formatError(e) })),
  });

  // v0.27.x: surface the script_status KV (per cmd_id ACTIVE/REVOKED)
  // alongside the job catalog so operators can SEE whether a revoke
  // landed instead of guessing. Empty map when the bucket is missing
  // — silently degrades to "everything is ACTIVE" which is the safe
  // pre-revoke default.
  const statusQuery = useQuery({
    queryKey: ['scripts-status'],
    queryFn: () => apiFetch<Record<string, string>>('/api/scripts/status'),
  });
  const statusMap = statusQuery.data ?? {};
  function isRevoked(id: string): boolean {
    return statusMap[id] === 'REVOKED';
  }

  // v0.27: surface Layer 2 revoke / unrevoke as per-row buttons so
  // operators don't have to drop to the CLI for a one-script gate
  // flip. Backend endpoint (POST /api/scripts/{cmd_id}/revoke) just
  // writes the script_status KV, which the agent's handle_command
  // reads at fire time. Idempotent on the server side — re-clicking
  // is a no-op put.
  //
  // Round 2 review (CodeRabbit #38): a single shared `useMutation`
  // overwrites `.variables` with every new invocation, so the
  // previous `disabled={isPending && variables === id}` flickered
  // back to enabled the moment a second row was clicked while the
  // first was still inflight. Track pending IDs in a `Set<string>`
  // updated in onMutate / onSettled — true per-row scoping that
  // survives concurrent clicks.
  const [pendingRevoke, setPendingRevoke] = useState<Set<string>>(new Set());
  const [pendingUnrevoke, setPendingUnrevoke] = useState<Set<string>>(new Set());
  const [pendingKill, setPendingKill] = useState<Set<string>>(new Set());

  // v0.32 / PR-B: Monaco-backed YAML editor for add / edit. Null when
  // the modal is closed; a fresh mode object opens it on the right
  // shape ({ type: 'create' } for the "New job" button, { type:
  // 'edit', id } for the per-row Edit button).
  const [editor, setEditor] = useState<EditorMode | null>(null);
  const revoke = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/scripts/${encodeURIComponent(id)}/revoke`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingRevoke((prev) => new Set(prev).add(id));
    },
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['scripts-status'] });
      toast.success(t('toast.revokeSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.revokeFailure', { error: formatError(e) })),
    onSettled: (_d, _e, id) => {
      setPendingRevoke((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
  });
  const unrevoke = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/scripts/${encodeURIComponent(id)}/unrevoke`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingUnrevoke((prev) => new Set(prev).add(id));
    },
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['scripts-status'] });
      toast.success(t('toast.unrevokeSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.unrevokeFailure', { error: formatError(e) })),
    onSettled: (_d, _e, id) => {
      setPendingUnrevoke((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
  });

  // v0.30 / PR γ: Layer 3 kill from the Jobs page. Distinct from
  // revoke (Layer 2) — kill stops the currently-running child
  // process for every in-flight exec of this cmd, but does NOT
  // prevent the next schedule tick from firing another fresh exec.
  // For "stop this job entirely", the operator clicks revoke
  // alongside (the confirm dialog mentions this so they don't
  // misread the scope). The backend's
  // `POST /api/jobs/{cmd_id}/kill` route (v0.29) does the
  // exec_id fan-out — pre-v0.29 it published `kill.{cmd_id}` to
  // an empty subject, which was a silent no-op.
  const kill = useMutation({
    mutationFn: (id: string) =>
      apiFetch(`/api/jobs/${encodeURIComponent(id)}/kill`, { method: 'POST' }),
    onMutate: (id) => {
      setPendingKill((prev) => new Set(prev).add(id));
    },
    // Refresh /api/jobs so the live chip recomputes once results
    // start landing post-kill (kills land as ExecResult exit_code
    // -1 → projector flips status from running → completed).
    onSuccess: (_d, id) => {
      qc.invalidateQueries({ queryKey: ['jobs'] });
      toast.success(t('toast.killSuccess', { id }));
    },
    onError: (e) => toast.error(t('toast.killFailure', { error: formatError(e) })),
    onSettled: (_d, _e, id) => {
      setPendingKill((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
  });

  function statusBadge(id: string) {
    return isRevoked(id) ? (
      <Badge variant="danger">{t('status.revoked')}</Badge>
    ) : (
      <Badge variant="success">{t('status.active')}</Badge>
    );
  }

  // v0.30 follow-up: compact icon + count chips so both running +
  // pending fit in a narrow column without wrapping to two lines.
  // Tooltips carry the full semantics. Stale `pending` rows (= fire
  // whose ExecResult never landed within 1 h) flip to `expired` via
  // the backend cleanup task and drop out of this chip automatically.
  function liveChips(j: JobRow) {
    if (j.live.running === 0 && j.live.pending === 0) {
      return <span className="text-muted text-xs">—</span>;
    }
    return (
      <div className="flex gap-1.5 items-center">
        {j.live.running > 0 && (
          // Deep-link into Activity pre-filtered to THIS job's in-flight
          // runs — the same status=running bridge the Dashboard failures
          // tile uses for status=failure, scoped to job_id so one click
          // goes from "3 running" to those three rows still executing.
          <Link
            to={`/activity?status=running&job_id=${encodeURIComponent(j.id)}`}
            title={t('live.runningTitle')}
            className="inline-flex"
            // The row's onClick opens the detail drawer — stop the chip
            // click from bubbling so it only navigates to Activity.
            onClick={(e) => e.stopPropagation()}
          >
            <Badge
              variant="violet"
              className="inline-flex items-center gap-1 px-1.5 cursor-pointer transition-opacity hover:opacity-80"
            >
              <Play className="size-3" />
              {j.live.running}
            </Badge>
          </Link>
        )}
        {j.live.pending > 0 && (
          <Badge
            variant="amber"
            title={t('live.pendingTitle')}
            className="inline-flex items-center gap-1 px-1.5"
          >
            <Hourglass className="size-3" />
            {j.live.pending}
          </Badge>
        )}
      </div>
    );
  }

  // One action strip, two render sites: icon-only inside the table
  // (rows stay one line tall) and icon+label inside the drawer
  // footer where there's room to spell the verbs out. Each action
  // renders ONLY when actionable for the current row state:
  //   * kill: shown only when something is in flight
  //   * revoke: shown only when active
  //   * unrevoke: shown only when revoked
  //   * delete: always (it's the last-resort op)
  function renderActions(j: JobRow, withLabels = false) {
    const inflight = j.live.running + j.live.pending;
    return (
      <>
        {/* Fire this job at the fleet. A revoked job is refused on the
            agents, so we hide the run shortcut while revoked (unrevoke
            shows in its place) rather than route the operator to an
            Exec that will bounce. The Link carries only job_id — Exec
            preselects the job and still makes the operator pick
            targets + confirm the blast radius. */}
        {!isRevoked(j.id) && (
          <Button variant="default" size="sm" asChild>
            <Link
              to={`/exec?job_id=${encodeURIComponent(j.id)}`}
              title={t('actions.runTitle')}
              aria-label={t('actions.runAria', { id: j.id })}
            >
              <Send className="size-3.5" />
              {withLabels && t('actions.run')}
            </Link>
          </Button>
        )}
        {inflight > 0 && (
          <Button
            variant="danger"
            size="sm"
            disabled={pendingKill.has(j.id)}
            onClick={async () => {
              const ok = await confirm({
                title: t('confirm.killTitle', { id: j.id }),
                description: t('confirm.killDescription', {
                  count: inflight,
                  running: j.live.running,
                  pending: j.live.pending,
                }),
                confirmLabel: t('confirm.killLabel'),
                danger: true,
              });
              if (ok) kill.mutate(j.id);
            }}
            title={t('actions.killTitle', { count: inflight })}
            aria-label={t('actions.killAria', { id: j.id })}
          >
            <Skull className="size-3.5" />
            {withLabels && t('actions.kill')}
          </Button>
        )}
        <Button
          variant="secondary"
          size="sm"
          onClick={() => setEditor({ type: 'edit', id: j.id })}
          title={t('actions.editTitle')}
          aria-label={t('actions.editAria', { id: j.id })}
        >
          <Pencil className="size-3.5" />
          {withLabels && t('actions.edit')}
        </Button>
        {!isRevoked(j.id) && (
          <Button
            variant="secondary"
            size="sm"
            disabled={pendingRevoke.has(j.id)}
            onClick={async () => {
              const ok = await confirm({
                title: t('confirm.revokeTitle', { id: j.id }),
                description: t('confirm.revokeDescription'),
                confirmLabel: t('confirm.revokeLabel'),
                danger: true,
              });
              if (ok) revoke.mutate(j.id);
            }}
            title={t('actions.revokeTitle')}
            aria-label={t('actions.revokeAria', { id: j.id })}
          >
            <Ban className="size-3.5" />
            {withLabels && t('actions.revoke')}
          </Button>
        )}
        {isRevoked(j.id) && (
          <Button
            variant="secondary"
            size="sm"
            disabled={pendingUnrevoke.has(j.id)}
            onClick={() => unrevoke.mutate(j.id)}
            title={t('actions.unrevokeTitle')}
            aria-label={t('actions.unrevokeAria', { id: j.id })}
          >
            <CircleCheck className="size-3.5" />
            {withLabels && t('actions.unrevoke')}
          </Button>
        )}
        <Button
          variant="danger"
          size="sm"
          disabled={del.isPending}
          onClick={async () => {
            const ok = await confirm({
              title: t('confirm.deleteTitle', { id: j.id }),
              description: t('confirm.deleteDescription'),
              confirmLabel: t('confirm.deleteLabel'),
              danger: true,
            });
            if (ok) del.mutate(j.id);
          }}
          title={t('actions.deleteTitle')}
          aria-label={t('actions.deleteAria', { id: j.id })}
        >
          <Trash2 className="size-3.5" />
          {withLabels && t('actions.delete')}
        </Button>
      </>
    );
  }

  // One table row. Extracted from the old inline `.map` so the
  // id-prefix groups below can each render their own slice without
  // duplicating the cell layout. The tag badges sit under the
  // description and are themselves filter toggles — clicking one
  // flips it in `activeTags` (stopPropagation so the row's drawer
  // doesn't also open).
  function renderJobRow(j: JobRow) {
    return (
      <TableRow
        key={j.id}
        tabIndex={0}
        className="cursor-pointer focus-visible:outline-none focus-visible:bg-muted/10"
        onClick={() => setSelectedId(j.id)}
        // Keyboard path for the clickable row. Guard on
        // currentTarget so Enter/Space pressed on a focused
        // action *button* (which bubbles its keydown up here)
        // doesn't also pop the drawer open.
        onKeyDown={(e) => {
          if (e.target === e.currentTarget && (e.key === 'Enter' || e.key === ' ')) {
            e.preventDefault();
            setSelectedId(j.id);
          }
        }}
        aria-label={t('row.openAria', { id: j.id })}
      >
        {/* `w-full max-w-0` — soak up whatever width is left
            after the fixed-content columns, truncating the
            description to fit (same trick the old 11-column
            layout used, now on the merged id+description
            cell). */}
        <TableCell className="w-full max-w-0">
          <div className="flex flex-col gap-0.5">
            <code className="text-xs font-medium">{j.id}</code>
            <span
              className="block truncate text-xs text-muted"
              title={j.description || undefined}
            >
              {j.description || '—'}
            </span>
            {j.tags && j.tags.length > 0 && (
              <div className="mt-0.5 flex flex-wrap gap-1" onClick={(e) => e.stopPropagation()}>
                {j.tags.map((tag) => (
                  <button
                    key={tag}
                    type="button"
                    onClick={() => toggleTag(tag)}
                    title={t('tags.filterByTitle', { tag })}
                    aria-pressed={activeTags.has(tag)}
                    className="cursor-pointer"
                  >
                    <Badge
                      variant={activeTags.has(tag) ? 'violet' : 'default'}
                      className="px-1.5 py-0 text-[10px] transition-colors hover:bg-violet/15 hover:text-violet"
                    >
                      {tag}
                    </Badge>
                  </button>
                ))}
              </div>
            )}
          </div>
        </TableCell>
        <TableCell>{statusBadge(j.id)}</TableCell>
        <TableCell>{liveChips(j)}</TableCell>
        {/* stopPropagation so clicking an action (or the dead
            space between buttons) doesn't also open the
            drawer underneath the confirm dialog. */}
        <TableCell onClick={(e) => e.stopPropagation()}>
          <div className="flex flex-nowrap gap-2">{renderActions(j)}</div>
        </TableCell>
      </TableRow>
    );
  }

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />{t('loading')}
      </div>
    );
  }
  if (error) return <ErrorCard title={t('errorTitle')} error={error} />;
  const rows = data ?? [];
  const selected = rows.find((j) => j.id === selectedId) ?? null;
  // Gemini review (#376): if a background refetch drops the selected
  // row (deleted from another session), the drawer closes
  // programmatically — onOpenChange never fires, so `selectedId`
  // would stay stale and re-clicking the same row would bail out of
  // the no-op setState without reopening the drawer. Adjust the
  // state during render so the next click is a real transition.
  if (selectedId !== null && selected === null) {
    setSelectedId(null);
  }

  // ---- id-prefix grouping + search / tag filtering (this PR) ----
  // The prefix is everything before the first hyphen (`install-tls` →
  // `install`). We bucket rows by it so the operator's existing naming
  // convention becomes free categories. Prefixes shared by only ONE
  // job aren't worth a header of their own, so they fold into a single
  // "その他" group — computed from the FULL set (not the filtered one)
  // so a row's group membership stays stable while you search.
  const OTHER = ' other'; // sentinel that can't collide with a real prefix
  const prefixOf = (id: string) => {
    const i = id.indexOf('-');
    return i > 0 ? id.slice(0, i) : id;
  };
  const prefixCounts = new Map<string, number>();
  for (const j of rows) {
    const p = prefixOf(j.id);
    prefixCounts.set(p, (prefixCounts.get(p) ?? 0) + 1);
  }
  const groupKeyOf = (id: string) => {
    const p = prefixOf(id);
    return (prefixCounts.get(p) ?? 0) >= 2 ? p : OTHER;
  };

  // Every distinct tag across the catalog, for the filter-chip row.
  const allTags = Array.from(new Set(rows.flatMap((j) => j.tags ?? []))).sort((a, b) =>
    a.localeCompare(b),
  );

  // Search matches id / description / any tag (case-insensitive); tag
  // filter is OR across the active set. Both must hold.
  const q = search.trim().toLowerCase();
  const matches = (j: JobRow) => {
    const tagHit =
      activeTags.size === 0 || (j.tags ?? []).some((tag) => activeTags.has(tag));
    if (!tagHit) return false;
    if (q === '') return true;
    return (
      j.id.toLowerCase().includes(q) ||
      (j.description ?? '').toLowerCase().includes(q) ||
      (j.tags ?? []).some((tag) => tag.toLowerCase().includes(q))
    );
  };
  const visibleRows = rows.filter(matches);
  const filtering = q !== '' || activeTags.size > 0;

  // Ordered groups: real prefixes alphabetically, then "その他" last.
  // Only groups with at least one visible row render.
  const realPrefixes = Array.from(prefixCounts.entries())
    .filter(([, n]) => n >= 2)
    .map(([p]) => p)
    .sort((a, b) => a.localeCompare(b));
  const orderedKeys = [...realPrefixes, OTHER];
  const groups = orderedKeys
    .map((key) => ({
      key,
      label: key === OTHER ? t('groups.other') : key,
      rows: visibleRows.filter((j) => groupKeyOf(j.id) === key),
    }))
    .filter((g) => g.rows.length > 0);

  if (rows.length === 0) {
    return (
      <>
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between">
              <CardTitle>{t('empty.title')}</CardTitle>
              <Button
                variant="default"
                size="sm"
                onClick={() => setEditor({ type: 'create' })}
              >
                <FilePlus2 className="size-3.5" />
                {t('newJob')}
              </Button>
            </div>
          </CardHeader>
          <CardContent className="text-muted">
            <Trans
              ns="jobs"
              i18nKey="empty.body"
              components={{
                code: <code />,
                strong: <strong />,
              }}
            />
          </CardContent>
        </Card>
        {editor !== null && (
          <YamlEditorDialog
            open
            onOpenChange={(next) => {
              if (!next) setEditor(null);
            }}
            kind="manifest"
            mode={editor}
          />
        )}
      </>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <div className="flex items-center gap-2">
          <Button
            variant="default"
            size="sm"
            onClick={() => setEditor({ type: 'create' })}
            title={t('newJobTitle')}
          >
            <FilePlus2 className="size-3.5" />
            {t('newJob')}
          </Button>
          <Badge variant="violet">
            {filtering ? `${visibleRows.length} / ${rows.length}` : rows.length}
          </Badge>
        </div>
      </div>
      {/* Filter bar: free-text search + (when any job is tagged) the
          tag-toggle chips. Both narrow the grouped table below. */}
      <div className="space-y-2">
        <div className="relative max-w-sm">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted" />
          <Input
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t('search.placeholder')}
            aria-label={t('search.placeholder')}
            className="pl-8 pr-8"
          />
          {search !== '' && (
            <button
              type="button"
              onClick={() => setSearch('')}
              title={t('search.clear')}
              aria-label={t('search.clear')}
              className="absolute right-2 top-1/2 -translate-y-1/2 text-muted hover:text-fg"
            >
              <X className="size-4" />
            </button>
          )}
        </div>
        {allTags.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5">
            <Tags className="size-4 text-muted" />
            {allTags.map((tag) => (
              <button
                key={tag}
                type="button"
                onClick={() => toggleTag(tag)}
                aria-pressed={activeTags.has(tag)}
                className="cursor-pointer"
              >
                <Badge
                  variant={activeTags.has(tag) ? 'violet' : 'default'}
                  className="transition-colors hover:bg-violet/15 hover:text-violet"
                >
                  {tag}
                </Badge>
              </button>
            ))}
            {activeTags.size > 0 && (
              <button
                type="button"
                onClick={() => setActiveTags(new Set())}
                className="text-xs text-muted hover:text-fg"
              >
                {t('tags.clear')}
              </button>
            )}
          </div>
        )}
      </div>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>{t('columns.job')}</TableHead>
            <TableHead>{t('columns.status')}</TableHead>
            <TableHead>{t('columns.live')}</TableHead>
            <TableHead>{t('columns.actions')}</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {groups.length === 0 ? (
            <TableRow>
              <TableCell colSpan={4} className="py-8 text-center text-sm text-muted">
                {t('noMatch')}
              </TableCell>
            </TableRow>
          ) : (
            groups.map((g) => {
              const isCollapsed = collapsed.has(g.key);
              return (
                <Fragment key={g.key}>
                  {/* Group header — clicking (or Enter/Space when
                      focused) toggles collapse for the whole prefix.
                      colSpan covers all four columns. */}
                  <TableRow
                    tabIndex={0}
                    role="button"
                    aria-expanded={!isCollapsed}
                    aria-label={`${isCollapsed ? t('groups.expand') : t('groups.collapse')} ${g.label}`}
                    className="cursor-pointer bg-muted/5 hover:bg-muted/10 focus-visible:outline-none focus-visible:bg-muted/10"
                    onClick={() => toggleGroup(g.key)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        toggleGroup(g.key);
                      }
                    }}
                  >
                    <TableCell colSpan={4} className="py-1.5">
                      <div className="flex items-center gap-1.5 text-xs font-medium">
                        {isCollapsed ? (
                          <ChevronRight className="size-3.5 text-muted" />
                        ) : (
                          <ChevronDown className="size-3.5 text-muted" />
                        )}
                        <span>{g.label}</span>
                        <Badge variant="default" className="px-1.5 py-0 text-[10px]">
                          {g.rows.length}
                        </Badge>
                      </div>
                    </TableCell>
                  </TableRow>
                  {!isCollapsed && g.rows.map((j) => renderJobRow(j))}
                </Fragment>
              );
            })
          )}
        </TableBody>
      </Table>
      <Sheet
        open={selected !== null}
        onOpenChange={(next) => {
          if (!next) setSelectedId(null);
        }}
      >
        {selected !== null && (
          <SheetContent>
            <SheetHeader>
              <SheetTitle>
                <code className="break-all">{selected.id}</code>
              </SheetTitle>
              <SheetDescription>
                {selected.description || t('detail.noDescription')}
              </SheetDescription>
            </SheetHeader>
            <div className="flex flex-wrap items-center gap-1.5">
              {statusBadge(selected.id)}
              {(selected.live.running > 0 || selected.live.pending > 0) && liveChips(selected)}
            </div>
            <DetailList>
              <DetailItem label={t('columns.version')}>
                <code className="text-xs">{selected.version}</code>
              </DetailItem>
              <DetailItem label={t('columns.shell')}>
                <code className="text-xs">{selected.execute.shell}</code>
              </DetailItem>
              <DetailItem label={t('columns.runAs')}>
                <code className="text-xs">{selected.execute.run_as ?? 'system'}</code>
              </DetailItem>
              <DetailItem label={t('columns.cwd')}>
                {selected.execute.cwd
                  ? <code className="text-xs break-all">{selected.execute.cwd}</code>
                  : <span className="text-muted text-xs">—</span>}
              </DetailItem>
              <DetailItem label={t('columns.timeout')}>
                <code className="text-xs">{selected.execute.timeout}</code>
              </DetailItem>
              <DetailItem label={t('columns.inventory')}>
                {selected.inventory
                  ? <Badge variant="violet"><ScrollText className="size-3" />{t('inventoryProbe')}</Badge>
                  : <span className="text-muted text-xs">—</span>}
              </DetailItem>
              <DetailItem label={t('columns.tags')}>
                {selected.tags && selected.tags.length > 0 ? (
                  <div className="flex flex-wrap gap-1">
                    {selected.tags.map((tag) => (
                      // Clickable filter toggle, same as the row tags —
                      // keeps "tags are filter affordances" consistent.
                      <button
                        key={tag}
                        type="button"
                        onClick={() => toggleTag(tag)}
                        title={t('tags.filterByTitle', { tag })}
                        aria-pressed={activeTags.has(tag)}
                        className="cursor-pointer"
                      >
                        <Badge
                          variant={activeTags.has(tag) ? 'violet' : 'default'}
                          className="px-1.5 py-0 text-[10px] transition-colors hover:bg-violet/15 hover:text-violet"
                        >
                          {tag}
                        </Badge>
                      </button>
                    ))}
                  </div>
                ) : (
                  <span className="text-muted text-xs">—</span>
                )}
              </DetailItem>
            </DetailList>
            <SheetFooter>
              <div className="flex flex-wrap justify-end gap-2">
                {renderActions(selected, true)}
              </div>
            </SheetFooter>
          </SheetContent>
        )}
      </Sheet>
      {del.error && <ErrorCard title={t('errors.deleteTitle')} error={del.error} />}
      {revoke.error && <ErrorCard title={t('errors.revokeTitle')} error={revoke.error} />}
      {unrevoke.error && <ErrorCard title={t('errors.unrevokeTitle')} error={unrevoke.error} />}
      {kill.error && <ErrorCard title={t('errors.killTitle')} error={kill.error} />}
      {editor !== null && (
        <YamlEditorDialog
          open
          onOpenChange={(next) => {
            if (!next) setEditor(null);
          }}
          kind="manifest"
          mode={editor}
        />
      )}
    </div>
  );
}
