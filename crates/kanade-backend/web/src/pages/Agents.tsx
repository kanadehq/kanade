import { keepPreviousData, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Activity, AlertTriangle, Loader2, ScrollText, Server, Settings2, SlidersHorizontal, Trash2, Users } from 'lucide-react';
import { useEffect, useState } from 'react';
import { Trans, useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router-dom';
import { toast } from 'sonner';

import { ErrorCard } from '@/components/ErrorCard';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useConfirm } from '@/components/ui/confirm-dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { JsonOutput } from '@/components/ui/json-output';
import { Select } from '@/components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, apiFetchPaged, formatError } from '@/lib/api';
import { useAuth } from '@/lib/auth';
import { useDebouncedValue } from '@/lib/hooks';
import type { AgentGroups, AgentRow, EffectiveConfigResponse, Heartbeat } from '@/lib/types';
import { cn, fmtIsoLocal, isAgentOnline, unresolvedQuarantine } from '@/lib/utils';

// #495: server-side page size. The endpoint supports q/limit/offset;
// 50 rows keeps the polled payload and the rendered DOM bounded
// regardless of fleet size (the page previously rendered the whole
// fleet every 30 s tick).
const PAGE_SIZE = 50;
// Same debounce the other list pages use for typed filters.
const FILTER_DEBOUNCE_MS = 300;
// #1051: localStorage key for the operator's chosen agent_meta columns
// (per-browser; a server-side per-account pref could follow later).
const META_COLS_KEY = 'agents.metaColumns';
// Built-in columns an operator can hide from the column picker (persisted
// per browser). `pc_id` is deliberately absent — it's the row identity +
// the link to the per-PC page, so it always shows. Order matches the
// table so the picker reads top-to-bottom like the header row.
const TOGGLEABLE_COLS = [
  'status',
  'os',
  'agent',
  'lastHeartbeat',
  'lastLogon',
  'cpu',
  'rss',
  'actions',
] as const;
const HIDDEN_COLS_KEY = 'agents.hiddenColumns';

/** Read a persisted `string[]` from localStorage, tolerating anything
 *  malformed. `getItem`/`JSON.parse` can throw (blocked storage, bad
 *  JSON); a valid-JSON but non-array value (a string / object left by
 *  other code or corruption) would otherwise slip through and crash later
 *  on `.includes()`. Falls back to `[]` in every failure mode and keeps
 *  only string entries. */
function readStringArray(key: string): string[] {
  try {
    const raw = localStorage.getItem(key);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) return parsed.filter((x): x is string => typeof x === 'string');
    }
  } catch {
    /* malformed / unavailable — fall through to the empty default */
  }
  return [];
}

// Liveness filter for the list, shared with the URL `?status=` param
// so the Dashboard's fleet-health tiles can deep-link straight to the
// offline hosts ("2 / 4 — which 2?"). `all` is the default / no-param
// state.
type StatusFilter = 'all' | 'online' | 'offline';

function parseStatusFilter(raw: string | null): StatusFilter {
  return raw === 'online' || raw === 'offline' ? raw : 'all';
}

type ActionResult = {
  pc_id: string;
  action: string;
  value: unknown;
};

/** v0.37 Part 2: per-cell formatters for the perf columns. Null
 *  values render as em-dash so pre-0.37 agents (which don't carry
 *  the fields) leave the column visibly blank rather than `0`. */
function fmtPct(v: number | null): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '—';
  return `${v.toFixed(1)}%`;
}

function fmtBytes(v: number | null): string {
  if (v === null || v === undefined || !Number.isFinite(v) || v < 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  let n = v;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n < 10 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}

export function Agents() {
  const { t } = useTranslation('agents');
  const queryClient = useQueryClient();
  const confirm = useConfirm();
  const { hasRole } = useAuth();
  const canOperate = hasRole('operator');
  const [q, setQ] = useState('');
  const [user, setUser] = useState('');
  const [version, setVersion] = useState('');
  const [offset, setOffset] = useState(0);
  const dQ = useDebouncedValue(q, FILTER_DEBOUNCE_MS);
  const dUser = useDebouncedValue(user, FILTER_DEBOUNCE_MS);
  const dVersion = useDebouncedValue(version, FILTER_DEBOUNCE_MS);
  // #1051: attribute (agent_meta) search — a key from the dropdown plus a
  // contains-value. `meta_value` only means anything with a `meta_key`.
  const [metaKey, setMetaKey] = useState('');
  const [metaValue, setMetaValue] = useState('');
  const dMetaValue = useDebouncedValue(metaValue, FILTER_DEBOUNCE_MS);
  // #1051: which agent_meta keys render as extra columns (persisted per
  // browser). Seeded from localStorage; kept in sync on every change.
  const [metaCols, setMetaCols] = useState<string[]>(() => readStringArray(META_COLS_KEY));
  useEffect(() => {
    // setItem can throw (SecurityError under blocked storage / some iframe
    // contexts, or QuotaExceeded) — a throw here would crash the render.
    try {
      localStorage.setItem(META_COLS_KEY, JSON.stringify(metaCols));
    } catch {
      /* non-persistent this session; not worth surfacing */
    }
  }, [metaCols]);
  // Distinct agent_meta keys across the fleet — drives the column picker
  // and the attribute-search key dropdown. Empty ⇒ no metadata anywhere,
  // so the controls stay hidden.
  const metaKeys = useQuery({
    queryKey: ['agent-meta-keys'],
    queryFn: () => apiFetch<string[]>('/api/agents/meta-keys'),
  }).data ?? [];
  const toggleMetaCol = (key: string) =>
    setMetaCols((prev) => (prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key]));
  const metaVal = (a: AgentRow, key: string) =>
    a.meta?.find((e) => e.key === key)?.value ?? '';
  // Render only selections that still exist fleet-wide. A key removed from
  // all PCs (or a transiently-empty key list) drops out of `metaKeys`, so
  // the picker can no longer offer it — filtering here stops such a
  // selection rendering a stuck, un-removable blank column. The raw
  // `metaCols` (localStorage) is kept intact so the choice returns if the
  // key reappears.
  const activeMetaCols = metaCols.filter((k) => metaKeys.includes(k));
  // Built-in columns the operator has hidden (persisted per browser). A
  // column is visible unless its id is in this set; pc_id is never here.
  const [hiddenCols, setHiddenCols] = useState<string[]>(() => readStringArray(HIDDEN_COLS_KEY));
  useEffect(() => {
    try {
      localStorage.setItem(HIDDEN_COLS_KEY, JSON.stringify(hiddenCols));
    } catch {
      /* non-persistent this session; not worth surfacing */
    }
  }, [hiddenCols]);
  const isColVisible = (id: string) => !hiddenCols.includes(id);
  const toggleCol = (id: string) =>
    setHiddenCols((prev) => (prev.includes(id) ? prev.filter((c) => c !== id) : [...prev, id]));
  const [searchParams, setSearchParams] = useSearchParams();
  const statusFilter = parseStatusFilter(searchParams.get('status'));
  // #652: the Rollout "quarantined K" drill-down lands here with
  // `?quarantined=<version>`. Read it from the URL (not local state) so
  // the deep link is shareable and survives a refresh; cleared via the
  // chip below.
  const quarantined = searchParams.get('quarantined') ?? '';
  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ['agents', dQ, dUser, dVersion, quarantined, offset, statusFilter, metaKey, dMetaValue],
    // Match the Dashboard cadence so the per-row online/offline badge
    // ages out a dropped agent within ~30s of the fleet-health tile.
    // #563: the status filter rides to the server, so the Dashboard's
    // `?status=offline` deep link pages over the whole fleet's
    // offline hosts instead of filtering the current page.
    // #652: q/user/version are server-side regexes; quarantined is an
    // exact version pre-filter. Each is appended only when non-empty so
    // the no-filter request stays on the SQL fast path.
    queryFn: () =>
      apiFetchPaged<AgentRow[]>(
        `/api/agents?limit=${PAGE_SIZE}&offset=${offset}` +
          (dQ ? `&q=${encodeURIComponent(dQ)}` : '') +
          (dUser ? `&user=${encodeURIComponent(dUser)}` : '') +
          (dVersion ? `&version=${encodeURIComponent(dVersion)}` : '') +
          (quarantined ? `&quarantined=${encodeURIComponent(quarantined)}` : '') +
          (statusFilter !== 'all' ? `&status=${statusFilter}` : '') +
          // #1051: attribute filter — value only rides when a key is set.
          (metaKey ? `&meta_key=${encodeURIComponent(metaKey)}` : '') +
          (metaKey && dMetaValue ? `&meta_value=${encodeURIComponent(dMetaValue)}` : ''),
      ),
    refetchInterval: 30_000,
    // Keep the previous page rendered while a filter keystroke changes
    // the queryKey, so `isLoading` only flips true on the very first
    // load. Without this the whole screen fell back to the loading
    // state on every keystroke (the `if (isLoading) return` below),
    // remounting the filter inputs and dropping focus mid-type — the
    // Activity page never had this because it swaps only the table on
    // isLoading, not the whole screen.
    placeholderData: keepPreviousData,
  });
  const total = data?.total ?? 0;
  const [result, setResult] = useState<ActionResult | null>(null);
  // #495 (was: one shared mutation.isPending greyed out the action
  // buttons on every row): track pending pc_ids in a Set so only the
  // row actually in flight disables — the pattern Jobs/Activity use.
  const [pendingPcs, setPendingPcs] = useState<Set<string>>(new Set());
  const markPending = (pcId: string, on: boolean) =>
    setPendingPcs((prev) => {
      const next = new Set(prev);
      if (on) next.add(pcId);
      else next.delete(pcId);
      return next;
    });
  // A new search (or status-chip change) resets to page 1 — a stale
  // offset against a narrower result set would show an empty page.
  // Adjusted during render (not useEffect) so the reset lands BEFORE
  // the query fires, avoiding one wasted fetch at the old offset
  // (PR #559 review, gemini + claude).
  const [prevFilterKey, setPrevFilterKey] = useState({
    dQ,
    dUser,
    dVersion,
    quarantined,
    statusFilter,
    metaKey,
    dMetaValue,
  });
  if (
    prevFilterKey.dQ !== dQ ||
    prevFilterKey.dUser !== dUser ||
    prevFilterKey.dVersion !== dVersion ||
    prevFilterKey.quarantined !== quarantined ||
    prevFilterKey.statusFilter !== statusFilter ||
    prevFilterKey.metaKey !== metaKey ||
    prevFilterKey.dMetaValue !== dMetaValue
  ) {
    setPrevFilterKey({ dQ, dUser, dVersion, quarantined, statusFilter, metaKey, dMetaValue });
    setOffset(0);
  }

  const setStatusFilter = (next: StatusFilter) => {
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        if (next === 'all') p.delete('status');
        else p.set('status', next);
        return p;
      },
      { replace: true },
    );
  };

  // #652: drop the quarantine drill-down filter, returning to the full
  // list. Other URL params (status) are preserved.
  const clearQuarantined = () => {
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        p.delete('quarantined');
        return p;
      },
      { replace: true },
    );
  };

  const ping = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch<{ heartbeat: Heartbeat }>(`/api/agents/${encodeURIComponent(pcId)}/ping`, {
        method: 'POST',
      }),
  });
  const effective = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch<EffectiveConfigResponse>(`/api/agents/${encodeURIComponent(pcId)}/effective_config`),
  });
  const groupsGet = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch<AgentGroups>(`/api/agents/${encodeURIComponent(pcId)}/groups`),
  });
  const groupsPut = useMutation({
    mutationFn: ({ pcId, groups }: { pcId: string; groups: string[] }) =>
      apiFetch<AgentGroups>(`/api/agents/${encodeURIComponent(pcId)}/groups`, {
        method: 'PUT',
        body: JSON.stringify({ groups }),
      }),
  });
  const del = useMutation({
    mutationFn: (pcId: string) =>
      apiFetch(`/api/agents/${encodeURIComponent(pcId)}`, { method: 'DELETE' }),
  });

  const doPing = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'ping', value: '…' });
    markPending(pcId, true);
    try {
      const r = await ping.mutateAsync(pcId);
      setResult({ pc_id: pcId, action: 'ping', value: r });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'ping', value: (e as Error).message });
    } finally {
      markPending(pcId, false);
    }
  };
  const doEffective = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'effective', value: '…' });
    markPending(pcId, true);
    try {
      const r = await effective.mutateAsync(pcId);
      setResult({ pc_id: pcId, action: 'effective', value: r });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'effective', value: (e as Error).message });
    } finally {
      markPending(pcId, false);
    }
  };
  const doDelete = async (pcId: string) => {
    const ok = await confirm({
      title: t('delete.confirmTitle', { pcId }),
      description: t('delete.confirmDescription'),
      confirmLabel: t('delete.confirmButton'),
      danger: true,
    });
    if (!ok) return;
    markPending(pcId, true);
    try {
      await del.mutateAsync(pcId);
      toast.success(t('delete.success', { pcId }));
      // Drop it from the rendered list immediately rather than waiting
      // for the 30s poll. Partial key matches every paged/​filtered
      // ['agents', …] query.
      void queryClient.invalidateQueries({ queryKey: ['agents'] });
    } catch (e) {
      toast.error(formatError(e));
    } finally {
      markPending(pcId, false);
    }
  };
  const doGroups = async (pcId: string) => {
    setResult({ pc_id: pcId, action: 'groups', value: t('groupsLoading') });
    markPending(pcId, true);
    try {
      const current = await groupsGet.mutateAsync(pcId);
      const next = window.prompt(
        t('groupsPrompt', {
          pcId,
          current: current.groups.join(', ') || t('groupsNone'),
        }),
        current.groups.join(', '),
      );
      if (next === null) {
        setResult({ pc_id: pcId, action: 'groups', value: t('groupsCancelled') });
        return;
      }
      const list = next.split(',').map((s) => s.trim()).filter(Boolean);
      const updated = await groupsPut.mutateAsync({ pcId, groups: list });
      setResult({ pc_id: pcId, action: 'groups', value: updated });
    } catch (e) {
      setResult({ pc_id: pcId, action: 'groups', value: (e as Error).message });
    } finally {
      markPending(pcId, false);
    }
  };

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-muted">
        <Loader2 className="size-4 animate-spin" />
        {t('loading')}
      </div>
    );
  }
  // Only take over the whole screen on the FIRST load failure. With
  // keepPreviousData a later error (e.g. typing an invalid regex → 400)
  // keeps the prior data, so we leave the filters mounted and show a
  // non-disruptive inline error below instead — otherwise an invalid
  // regex mid-type unmounts the inputs and the user can't backspace to
  // fix it (gemini #666).
  if (error && !data) return <ErrorCard title={t('errorTitle')} error={error} />;
  // #563: rows arrive pre-filtered from the server; the fleet-wide
  // per-status counts ride the response headers so the chips stay
  // correct whichever chip is active (previously page-local).
  const visible = data?.rows ?? [];
  const onlineCount = data?.online ?? 0;
  const offlineCount = data?.offline ?? 0;
  // Headers missing (older backend) → fall back to the total so the
  // "All" chip never shows a bogus 0 against a populated table.
  const allCount =
    data?.online !== undefined && data?.offline !== undefined
      ? onlineCount + offlineCount
      : total;
  // One `now` snapshot for the whole render so the per-row badges
  // below agree on liveness for an agent sitting on the threshold.
  const now = Date.now();

  // Only the genuinely-empty fleet gets the onboarding card — a
  // filtered-empty page (search, status chip, or an out-of-range
  // offset) keeps the table chrome so the operator can clear the
  // filter / page back.
  if (total === 0 && !dQ && !dUser && !dVersion && !quarantined && statusFilter === 'all' && !metaKey) {
    return (
      <Card>
        <CardHeader className="items-center text-center">
          <Server className="size-10 text-muted" />
          <CardTitle>{t('empty.title')}</CardTitle>
        </CardHeader>
        <CardContent className="text-center text-muted">
          {t('empty.body')}
        </CardContent>
      </Card>
    );
  }

  // Visible column count for the empty-state colSpan: pc_id (always) +
  // the un-hidden built-ins + the active metadata columns.
  const colCount =
    1 + TOGGLEABLE_COLS.filter((c) => isColVisible(c)).length + activeMetaCols.length;

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <div className="flex items-center gap-2">
          <h2 className="text-xl">{t('title')}</h2>
          {/* In-flight cue for a filter query: with keepPreviousData the
              table keeps showing prior rows while the new query runs, so
              without this the refresh is invisible (claude #666). Mirrors
              the Activity page's isFetching spinner. */}
          {isFetching && !isLoading && (
            <Loader2 className="size-4 animate-spin text-muted" />
          )}
        </div>
        <Badge variant="violet">{t('countBadge', { count: total })}</Badge>
      </div>
      <p className="text-xs text-muted">
        <Trans
          ns="agents"
          i18nKey="intro"
          components={{
            inventoryLink: <Link to="/inventory" className="underline" />,
            strong: <strong />,
          }}
        />
      </p>

      {/* Liveness filter — the bridge to the Dashboard's "active /
          known" tile. Counts come from the same `isAgentOnline`
          threshold, so toggling "offline" answers "which hosts are
          the N that aren't connected?" directly. */}
      {/* #652: three regex filters, each LABELLED like the Activity
          page — without a label an empty box is just a mystery box. */}
      <div className="flex flex-wrap gap-3">
        <div className="space-y-1">
          <Label htmlFor="agents-q">{t('search.pcHostLabel')}</Label>
          <Input
            id="agents-q"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder={t('search.pcHost')}
            className="h-8 w-56"
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor="agents-user">{t('search.userLabel')}</Label>
          <Input
            id="agents-user"
            value={user}
            onChange={(e) => setUser(e.target.value)}
            placeholder={t('search.user')}
            className="h-8 w-44"
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor="agents-version">{t('search.versionLabel')}</Label>
          <Input
            id="agents-version"
            value={version}
            onChange={(e) => setVersion(e.target.value)}
            placeholder={t('search.version')}
            className="h-8 w-40"
          />
        </div>
        {/* #1051: attribute (agent_meta) search — only when metadata
            exists anywhere in the fleet. */}
        {metaKeys.length > 0 && (
          <div className="space-y-1">
            <Label htmlFor="agents-meta-key">{t('search.attributeLabel')}</Label>
            <div className="flex gap-1">
              <Select
                id="agents-meta-key"
                value={metaKey}
                onChange={(e) => setMetaKey(e.target.value)}
                className="h-8 w-36"
              >
                <option value="">{t('search.attributeAny')}</option>
                {metaKeys.map((k) => (
                  <option key={k} value={k}>
                    {k}
                  </option>
                ))}
              </Select>
              <Input
                value={metaValue}
                onChange={(e) => setMetaValue(e.target.value)}
                placeholder={t('search.attributeValue')}
                className="h-8 w-40"
                disabled={!metaKey}
              />
            </div>
          </div>
        )}
        {/* Column picker — always available: hide/show the built-in
            columns (pc_id is always shown), plus check agent_meta keys on
            as extra columns when any metadata exists. */}
        <div className="space-y-1">
          <Label>{t('columns.label')}</Label>
          <details className="relative">
            <summary className="flex h-8 cursor-pointer list-none items-center gap-1.5 rounded-md border border-border px-2.5 text-sm hover:bg-muted/10">
              <SlidersHorizontal className="size-3.5" />
              {t('columns.pick')}
            </summary>
            <div className="absolute right-0 z-20 mt-1 max-h-72 w-56 overflow-auto rounded-md border border-border bg-card p-2 shadow-lg">
              <div className="px-1 pb-1 text-[10px] font-medium uppercase tracking-wide text-muted">
                {t('columns.builtinSection')}
              </div>
              {TOGGLEABLE_COLS.map((id) => (
                <label
                  key={id}
                  className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 text-sm hover:bg-muted/10"
                >
                  <input type="checkbox" checked={isColVisible(id)} onChange={() => toggleCol(id)} />
                  <span className="truncate">{t(`columns.${id}`)}</span>
                </label>
              ))}
              {metaKeys.length > 0 && (
                <>
                  <div className="mt-2 px-1 pb-1 text-[10px] font-medium uppercase tracking-wide text-muted">
                    {t('columns.attributesLabel')}
                  </div>
                  {metaKeys.map((k) => (
                    <label
                      key={k}
                      className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 text-sm hover:bg-muted/10"
                    >
                      <input
                        type="checkbox"
                        checked={metaCols.includes(k)}
                        onChange={() => toggleMetaCol(k)}
                      />
                      <span className="truncate">{k}</span>
                    </label>
                  ))}
                </>
              )}
            </div>
          </details>
        </div>
      </div>

      {/* Liveness chips + active quarantine drill-down. */}
      <div className="flex flex-wrap items-center gap-2">
        {(['all', 'online', 'offline'] as const).map((s) => {
          // #563: every chip shows its fleet-wide (q-matching) count
          // from the response headers — no longer page-local.
          const count =
            s === 'online' ? onlineCount
            : s === 'offline' ? offlineCount
            : allCount;
          const selected = statusFilter === s;
          return (
            <button
              key={s}
              type="button"
              onClick={() => setStatusFilter(s)}
              aria-pressed={selected}
              className={cn(
                'rounded-full border px-3 py-1 text-xs font-medium transition-colors',
                selected
                  ? s === 'offline'
                    ? 'border-transparent bg-danger/15 text-danger'
                    : s === 'online'
                      ? 'border-transparent bg-success/15 text-success'
                      : 'border-transparent bg-violet/15 text-violet'
                  : 'border-border text-muted hover:bg-muted/10',
              )}
            >
              {t(`filter.${s}`, { count })}
            </button>
          );
        })}
        {/* #652: active quarantine drill-down, arrived via the Rollout
            link. Clearable to return to the full list. */}
        {quarantined && (
          <button
            type="button"
            onClick={clearQuarantined}
            className="inline-flex items-center gap-1 rounded-full border border-danger/40 bg-danger/10 px-3 py-1 text-xs font-medium text-danger transition-colors hover:bg-danger/20"
            title={t('quarantinedChipTitle')}
          >
            <AlertTriangle className="size-3" />
            {t('quarantinedChip', { version: quarantined })}
            <span aria-hidden>✕</span>
          </button>
        )}
      </div>
      {/* Non-disruptive error (e.g. an invalid-regex 400) — the filters
          above stay mounted so the user can fix the pattern in place. */}
      {error && (
        <p className="text-xs text-danger">
          {t('errorTitle')}: {(error as Error).message}
        </p>
      )}
      <Table>
        <TableHeader>
          <TableRow>
            {isColVisible('status') && <TableHead>{t('columns.status')}</TableHead>}
            {/* pc_id is usually COMPUTERNAME lower-cased, so a separate
                hostname column duplicated it. The hostname now rides the
                pc_id cell, shown only when it genuinely differs. pc_id is
                the row identity + detail link, so it's never hideable. */}
            <TableHead>{t('columns.pcId')}</TableHead>
            {isColVisible('os') && <TableHead>{t('columns.os')}</TableHead>}
            {isColVisible('agent') && <TableHead>{t('columns.agent')}</TableHead>}
            {isColVisible('lastHeartbeat') && (
              <TableHead>{t('columns.lastHeartbeat')}</TableHead>
            )}
            {isColVisible('lastLogon') && (
              <TableHead title={t('columnTitles.lastLogon')}>{t('columns.lastLogon')}</TableHead>
            )}
            {/* #1051: operator-selected agent_meta columns. */}
            {activeMetaCols.map((k) => (
              <TableHead key={k} title={t('columnTitles.attribute', { key: k })}>
                {k}
              </TableHead>
            ))}
            {/* v0.37 Part 2: agent process self-perf columns. Pre-
                0.37 agents leave these null and the cell renders
                as an em-dash, so the table stays usable during a
                rolling upgrade. Headers are prefixed "agent" so the
                columns are clearly the agent process, not the host. */}
            {isColVisible('cpu') && (
              <TableHead className="text-right" title={t('columnTitles.cpu')}>
                {t('columns.cpu')}
              </TableHead>
            )}
            {isColVisible('rss') && (
              <TableHead className="text-right" title={t('columnTitles.rss')}>
                {t('columns.rss')}
              </TableHead>
            )}
            {isColVisible('actions') && <TableHead>{t('columns.actions')}</TableHead>}
          </TableRow>
        </TableHeader>
        <TableBody>
          {visible.map((a) => {
            const online = isAgentOnline(a.last_heartbeat, now);
            // #652 follow-up: only quarantine entries NEWER than the
            // running version are unresolved. An older rolled-back
            // version is healed once the agent adopted a newer build, so
            // it must not keep raising a badge — otherwise a 3000-host
            // fleet carries stale badges with no fleet-scale way to clear
            // them.
            const unresolvedVersions = unresolvedQuarantine(a.quarantined_versions, a.agent_version);
            return (
            <TableRow key={a.pc_id}>
              {isColVisible('status') && (
                <TableCell label={t('columns.status')}>
                  <div className="flex flex-col items-start gap-1">
                    <Badge
                      variant={online ? 'success' : 'danger'}
                      title={t(online ? 'status.onlineTitle' : 'status.offlineTitle')}
                    >
                      <span
                        className={cn(
                          'mr-1.5 inline-block size-1.5 rounded-full',
                          online ? 'bg-success' : 'bg-danger',
                        )}
                      />
                      {t(online ? 'status.online' : 'status.offline')}
                    </Badge>
                    {/* #652: per-row quarantine badge so the drill-down
                        list is actionable — the version(s) this host
                        rolled back show on hover. */}
                    {unresolvedVersions.length > 0 && (
                      <Badge
                        variant="danger"
                        title={t('quarantinedBadgeTitle', {
                          versions: unresolvedVersions.join(', '),
                        })}
                      >
                        <AlertTriangle className="mr-1 size-3" />
                        {t('quarantinedBadge')}
                      </Badge>
                    )}
                  </div>
                </TableCell>
              )}
              <TableCell label={t('columns.pcId')}>
                <Link
                  to={`/agents/${encodeURIComponent(a.pc_id)}`}
                  className="hover:underline"
                  title={t('rowLinkTitle')}
                >
                  <code className="text-xs">{a.pc_id}</code>
                </Link>
                {/* Show hostname only when it differs from pc_id beyond
                    casing — otherwise it's the same value (e.g. MINIPC
                    vs minipc) and just noise. */}
                {a.hostname &&
                  a.hostname.toLowerCase() !== a.pc_id.toLowerCase() && (
                    <div className="text-muted text-[10px]">{a.hostname}</div>
                  )}
              </TableCell>
              {isColVisible('os') && (
                <TableCell label={t('columns.os')} className="text-muted text-xs">{a.os_family ?? '—'}</TableCell>
              )}
              {isColVisible('agent') && (
                <TableCell label={t('columns.agent')} className="text-muted text-xs">{a.agent_version ?? '—'}</TableCell>
              )}
              {isColVisible('lastHeartbeat') && (
                <TableCell label={t('columns.lastHeartbeat')} className="text-muted text-xs">{fmtIsoLocal(a.last_heartbeat)}</TableCell>
              )}
              {isColVisible('lastLogon') && (
                <TableCell label={t('columns.lastLogon')} className="text-xs">
                  {a.last_logon_display_name || a.last_logon_user ? (
                    <div className="flex flex-col">
                      {/* Display name as the primary line; fall back to the
                          login name when there's no display name (common for
                          local / domain accounts where LastLoggedOnDisplayName
                          is an EMPTY STRING, not null — so `||`, not `??`,
                          which would render the empty string). Show the login
                          name as a secondary line only when a display name is
                          actually present. */}
                      <span>{a.last_logon_display_name || a.last_logon_user}</span>
                      {a.last_logon_display_name && a.last_logon_user && (
                        <code className="text-muted text-[10px]">{a.last_logon_user}</code>
                      )}
                    </div>
                  ) : (
                    <span className="text-muted">—</span>
                  )}
                </TableCell>
              )}
              {/* #1051: operator-selected agent_meta cells. */}
              {activeMetaCols.map((k) => {
                const v = metaVal(a, k);
                return (
                  <TableCell key={k} label={k} className="text-xs">
                    {v || <span className="text-muted">—</span>}
                  </TableCell>
                );
              })}
              {isColVisible('cpu') && (
                <TableCell label={t('columns.cpu')} className="text-right text-muted text-xs">{fmtPct(a.agent_cpu_pct)}</TableCell>
              )}
              {isColVisible('rss') && (
                <TableCell label={t('columns.rss')} className="text-right text-muted text-xs">{fmtBytes(a.agent_rss_bytes)}</TableCell>
              )}
              {isColVisible('actions') && (
                <TableCell>
                  <div className="flex gap-1 flex-wrap">
                    <Button variant="secondary" size="sm" asChild>
                      <Link to={`/inventory?pc=${encodeURIComponent(a.pc_id)}`}>
                        <ScrollText className="size-3.5" />{t('actions.facts')}
                      </Link>
                    </Button>
                    <Button variant="secondary" size="sm" onClick={() => doPing(a.pc_id)} disabled={pendingPcs.has(a.pc_id)}>
                      <Activity className="size-3.5" />{t('actions.ping')}
                    </Button>
                    <Button variant="secondary" size="sm" onClick={() => doGroups(a.pc_id)} disabled={pendingPcs.has(a.pc_id)}>
                      <Users className="size-3.5" />{t('actions.groups')}
                    </Button>
                    <Button variant="secondary" size="sm" onClick={() => doEffective(a.pc_id)} disabled={pendingPcs.has(a.pc_id)}>
                      <Settings2 className="size-3.5" />{t('actions.effective')}
                    </Button>
                    <Button
                      variant="danger"
                      size="sm"
                      onClick={() => doDelete(a.pc_id)}
                      disabled={!canOperate || pendingPcs.has(a.pc_id)}
                      title={canOperate ? undefined : t('rbac.operatorRequired', { ns: 'common' })}
                    >
                      <Trash2 className="size-3.5" />{t('actions.delete')}
                    </Button>
                  </div>
                </TableCell>
              )}
            </TableRow>
            );
          })}
          {visible.length === 0 && (
            <TableRow>
              <TableCell colSpan={colCount} className="text-center text-muted text-sm py-6">
                {t('filterEmpty')}
              </TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>

      {(offset > 0 || total > PAGE_SIZE) && (
        <div className="flex items-center gap-2">
          <Button
            variant="secondary"
            size="sm"
            onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
            disabled={offset === 0}
          >
            {t('prev')}
          </Button>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => setOffset(offset + PAGE_SIZE)}
            disabled={offset + PAGE_SIZE >= total}
          >
            {t('next')}
          </Button>
          <span className="text-xs text-muted">
            {t('pageRange', {
              from: Math.min(offset + 1, total),
              to: Math.min(offset + PAGE_SIZE, total),
              total,
            })}
          </span>
        </div>
      )}

      {result && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">
              <code className="text-xs mr-2">{result.pc_id}</code>
              <Badge variant="amber">{result.action}</Badge>
            </CardTitle>
          </CardHeader>
          <CardContent>
            <JsonOutput value={result.value} />
          </CardContent>
        </Card>
      )}
    </div>
  );
}
